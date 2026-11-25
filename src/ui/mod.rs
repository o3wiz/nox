mod components;
mod icons;
mod resize;
mod scrollbar;
mod selectable_text;
mod theme;

use components::{
    ButtonTone, TOOLTIP_DELAY, Tooltip, format_bytes, format_count, icon_button, search_toggle,
    toolbar_divider,
};
use gpui::{prelude::*, *};
use gpui_ce_elements::editable_text::{
    EditableTextState, StringStorage, TextChanged,
    actions::{self as input_actions, DEFAULT_INPUT_CONTEXT, default_bindings},
    text_input,
};
use icons::Icon;
use nox::{
    config::{self, Command, Config, SearchConfig, SearchMode},
    fuzzy::{self, PresetMatch},
    logfile::{LogFile, Stamp},
    path_complete::{
        CompletionResult, PathCompletion, complete_path, ensure_trailing_separator, home_dir,
        resolve_input_path,
    },
    search::{self, SearchResults},
    selection::{LineSelection, TextPosition, TextSelection, word_range_at},
    session::{self, SavedTab, Session},
    zoom::{
        DEFAULT_ZOOM_PERCENT, MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT, ZoomMetrics, clamp_zoom_percent,
        scaled_scroll_offset, zoom_in_percent, zoom_out_percent,
    },
};
use regex::Regex;
use resize::{
    FILE_PATH_BAR_HEIGHT, RESIZE_SASH_HEIGHT, ResizeDirection, ResizeMotion, ResultsResizeDrag,
    height_for_drag, is_main_hidden, is_results_hidden, results_height_bounds,
};
use scrollbar::{
    DEFAULT_SCROLLBAR_WIDTH, HORIZONTAL_BAR_HEIGHT, ScrollAxis, ScrollbarTrackState,
    resolve_track_press, scroll_offset_for_drag, scrollbar_canvas, set_uniform_list_axis_offset,
};
use selectable_text::{RowLayoutCache, SelectableRowText, TextPressEvent};
use serde::Deserialize;
use std::{
    cell::Cell,
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};
use theme::{ICON_HERO, ICON_MD, ICON_SM, ICON_XS, Theme};

#[derive(Clone, PartialEq, Deserialize, schemars::JsonSchema, Action)]
#[action(namespace = nox)]
struct Execute {
    command: Command,
}

pub fn keybindings(config: &Config) -> anyhow::Result<Vec<KeyBinding>> {
    let mut parsed = Vec::with_capacity(config.keyboard.bindings.len());
    let mut explicit = HashSet::new();
    for binding in &config.keyboard.bindings {
        let loaded = KeyBinding::load(
            &binding.keys,
            Box::new(Execute {
                command: binding.action,
            }),
            Some(KeyBindingContextPredicate::parse(binding.context.predicate())?.into()),
            false,
            None,
            &DummyKeyboardMapper,
        )?;
        if loaded.keystrokes().len() == 1 {
            let ks = loaded.keystrokes()[0].inner();
            explicit.insert((binding.context, ks.modifiers, ks.key.clone()));
        }
        parsed.push((binding, loaded));
    }

    // Preserve user order within equal specificity while ensuring compound/mode contexts
    // (such as `TextSelected && RegexMode` over `TextSelected`) always take precedence.
    parsed.sort_by_key(|(binding, _)| binding.context.specificity());

    let mut out = Vec::with_capacity(parsed.len() + 4);
    for (binding, loaded) in parsed {
        // On macOS (and X11/Windows), pressing Shift + '-' folds Shift into '_' and Shift + '='
        // folds Shift into '+', clearing the shift modifier. Register automatic companion aliases
        // so bindings match both simulated shifted keystrokes and native folded keystrokes, and
        // allow `ZoomIn` on `secondary-=` to also respond to `secondary-+` / `secondary-shift-=`,
        // unless the user explicitly bound those combinations in the same context.
        let mut alias_candidates: Vec<(Modifiers, &'static str)> = Vec::new();
        if loaded.keystrokes().len() == 1 {
            let ks = loaded.keystrokes()[0].inner();
            if ks.modifiers.shift && ks.key == "-" {
                let mut unshifted = ks.modifiers;
                unshifted.shift = false;
                alias_candidates.push((unshifted, "_"));
            } else if ks.modifiers.shift && ks.key == "=" {
                let mut unshifted = ks.modifiers;
                unshifted.shift = false;
                alias_candidates.push((unshifted, "+"));
            } else if binding.action == Command::ZoomIn && !ks.modifiers.shift && ks.key == "=" {
                alias_candidates.push((ks.modifiers, "+"));
                let mut shifted = ks.modifiers;
                shifted.shift = true;
                alias_candidates.push((shifted, "="));
            }
        }
        out.push(loaded);
        for (alias_mods, alias_key) in alias_candidates {
            if explicit.insert((binding.context, alias_mods, alias_key.to_owned())) {
                let alias_ks = Keystroke {
                    modifiers: alias_mods,
                    key: alias_key.to_owned(),
                    key_char: None,
                };
                out.push(KeyBinding::load(
                    &alias_ks.unparse(),
                    Box::new(Execute {
                        command: binding.action,
                    }),
                    Some(KeyBindingContextPredicate::parse(binding.context.predicate())?.into()),
                    false,
                    None,
                    &DummyKeyboardMapper,
                )?);
            }
        }
    }
    Ok(out)
}

fn sanitize_single_line_search_text(text: &str) -> String {
    text.replace(['\r', '\n'], "")
}

fn append_regex_alternative(existing: &str, selected: &str) -> String {
    if existing.is_empty() {
        selected.to_owned()
    } else if existing.ends_with('|') {
        format!("{existing}{selected}")
    } else {
        format!("{existing}|{selected}")
    }
}

const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BULK_BOOKMARK_LINES: usize = 100_000;
const SEARCH_INDICATOR_DELAY: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Main,
    Results,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScrollbarId {
    pane: Pane,
    axis: ScrollAxis,
}

#[derive(Clone, Copy, Debug)]
struct ScrollbarDrag {
    id: ScrollbarId,
    grab_offset: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextDragGranularity {
    Char,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug)]
struct TextDrag {
    pane: Pane,
    granularity: TextDragGranularity,
    anchor_start: TextPosition,
    anchor_end: TextPosition,
}

struct Tab {
    id: usize,
    path: PathBuf,
    log: Option<Arc<LogFile>>,
    loading: bool,
    searching: bool,
    searching_since: Option<Instant>,
    searching_visible: bool,
    load_cancel: Arc<AtomicBool>,
    search_cancel: Arc<AtomicBool>,
    revision: usize,
    query: String,
    options: SearchConfig,
    highlight_regex: Option<Regex>,
    results: SearchResults,
    bookmarks: BTreeSet<usize>,
    selected: usize,
    main_selection: LineSelection,
    result_selection: LineSelection,
    main_text_selection: Option<TextSelection>,
    result_text_selection: Option<TextSelection>,
    follow: bool,
    scroll: UniformListScrollHandle,
    result_scroll: UniformListScrollHandle,
    error: Option<String>,
}

impl Tab {
    fn sync_highlight_regex(&mut self) {
        self.highlight_regex = if self.query.is_empty() || self.options.invert {
            None
        } else {
            search::compile(&self.query, &self.options).ok()
        };
    }

    fn text_selection(&self, pane: Pane) -> Option<TextSelection> {
        match pane {
            Pane::Main => self.main_text_selection,
            Pane::Results => self.result_text_selection,
        }
        .filter(|selection| !selection.is_empty())
    }

    fn text_selection_mut(&mut self, pane: Pane) -> &mut Option<TextSelection> {
        match pane {
            Pane::Main => &mut self.main_text_selection,
            Pane::Results => &mut self.result_text_selection,
        }
    }

    fn has_text_selection(&self, pane: Pane) -> bool {
        self.text_selection(pane).is_some()
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        self.load_cancel.store(true, Ordering::Relaxed);
        self.search_cancel.store(true, Ordering::Relaxed);
    }
}

enum Message {
    Loaded(usize, Result<Arc<LogFile>, String>),
    Exported(Result<(), String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusTone {
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusMessage {
    tone: StatusTone,
    text: String,
}

impl StatusMessage {
    fn success(text: impl Into<String>) -> Self {
        Self {
            tone: StatusTone::Success,
            text: text.into(),
        }
    }

    fn warning(text: impl Into<String>) -> Self {
        Self {
            tone: StatusTone::Warning,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            tone: StatusTone::Error,
            text: text.into(),
        }
    }

    fn icon_and_color(&self, theme: &Theme) -> (Icon, Hsla) {
        match self.tone {
            StatusTone::Success => (Icon::Check, theme.accent),
            StatusTone::Warning => (Icon::AlertTriangle, theme.warning),
            StatusTone::Error => (Icon::AlertTriangle, theme.danger),
        }
    }
}

pub struct Workspace {
    config: Config,
    theme: Theme,
    highlights: Vec<(Regex, u32)>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    input: Entity<EditableTextState>,
    path_input: Entity<EditableTextState>,
    last_handled_path_version: u16,
    path_completions: CompletionResult,
    path_completion_index: Option<usize>,
    show_path_completions: bool,
    path_completions_scroll: ScrollHandle,
    path_bar_bounds: Rc<Cell<Bounds<Pixels>>>,
    goto: Entity<EditableTextState>,
    show_goto: bool,
    show_presets: bool,
    preset_index: usize,
    presets_search: Entity<EditableTextState>,
    last_handled_presets_search_version: u16,
    filtered_presets: Vec<PresetMatch>,
    presets_focus: FocusHandle,
    presets_scroll: ScrollHandle,
    presets_trigger_bounds: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
    results_focus: FocusHandle,
    active_pane: Pane,
    drag_selecting: Option<Pane>,
    text_drag: Option<TextDrag>,
    main_row_layouts: RowLayoutCache,
    results_row_layouts: RowLayoutCache,
    render_frame: u64,
    scrollbar_drag: Option<ScrollbarDrag>,
    results_height: f32,
    target_results_height: f32,
    zoom_percent: u16,
    results_resize_drag: Option<ResultsResizeDrag>,
    resize_motion: ResizeMotion,
    resize_frame_scheduled: bool,
    main_vscroll_state: ScrollbarTrackState,
    main_hscroll_state: ScrollbarTrackState,
    results_vscroll_state: ScrollbarTrackState,
    results_hscroll_state: ScrollbarTrackState,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    last_poll: Instant,
    status: Option<StatusMessage>,
    _timer: Task<()>,
    _subscriptions: Vec<Subscription>,
    state_path: Option<PathBuf>,
    recent: Vec<PathBuf>,
    show_files: bool,
    header_click_armed: bool,
    #[cfg(test)]
    header_zoom_count: usize,
}

impl Workspace {
    fn new(
        config: Config,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let focus = cx.focus_handle();
        let results_focus = cx.focus_handle();
        let presets_focus = cx.focus_handle();
        focus.focus(window, cx);
        let timer = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(80))
                    .await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        });
        let highlights = if config.highlighting.enabled {
            config
                .highlighting
                .rules
                .iter()
                .map(|rule| {
                    (
                        regex::RegexBuilder::new(&rule.pattern)
                            .case_insensitive(!rule.case_sensitive)
                            .build()
                            .unwrap(),
                        config::color(&rule.color).unwrap(),
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let state_path = session::state_path(&config.files);
        let mut session_error = None;
        let saved = if config.files.save_session || config.files.restore_session {
            state_path
                .as_ref()
                .map(|path| Session::load(path, &config))
                .transpose()
                .unwrap_or_else(|error| {
                    session_error = Some(error.to_string());
                    None
                })
                .unwrap_or_default()
        } else {
            Session::default()
        };
        let restore = paths.is_empty() && config.files.restore_session;
        let paths = if restore {
            saved.tabs.iter().map(|tab| tab.path.clone()).collect()
        } else {
            paths
        };
        let results_height = if config.files.restore_session {
            saved.results_height.unwrap_or(config.window.results_height)
        } else {
            config.window.results_height
        };
        let zoom_percent = if config.files.restore_session {
            saved
                .zoom_percent
                .map(clamp_zoom_percent)
                .unwrap_or(DEFAULT_ZOOM_PERCENT)
        } else {
            DEFAULT_ZOOM_PERCENT
        };
        let path_input = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let path_focus = path_input.read(cx).focus_handle(cx);
        let last_handled_path_version = path_input.read(cx).version();
        let presets_search = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let last_handled_presets_search_version = presets_search.read(cx).version();
        let filtered_presets = fuzzy::filter_presets(&config.search.presets, "");
        let subscriptions = vec![
            cx.on_app_quit(|this, cx| {
                this.save_session(cx);
                async {}
            }),
            cx.on_release(|this, cx| this.save_session(cx)),
            cx.subscribe_in(
                &path_input,
                window,
                |this, _, _: &TextChanged, window, cx| {
                    this.on_path_input_changed(window, cx);
                },
            ),
            cx.subscribe_in(
                &presets_search,
                window,
                |this, _, _: &TextChanged, window, cx| {
                    this.on_presets_search_changed(window, cx);
                },
            ),
            cx.on_focus(&path_focus, window, |this, _window, cx| {
                this.show_goto = false;
                this.dismiss_presets(cx);
                cx.notify();
            }),
            cx.on_blur(&path_focus, window, |this, _window, cx| {
                this.on_path_input_blur(cx);
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.header_click_armed = false;
                    this.resize_motion.release_hold();
                    let mut changed = false;
                    if this.results_resize_drag.take().is_some() {
                        changed = true;
                    }
                    if this.text_drag.take().is_some() {
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }
            }),
        ];
        let theme = Theme::from_config(&config, cx);
        let mut this = Self {
            theme,
            config,
            highlights,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            input: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            path_input,
            last_handled_path_version,
            path_completions: CompletionResult::default(),
            path_completion_index: None,
            show_path_completions: false,
            path_completions_scroll: ScrollHandle::new(),
            path_bar_bounds: Rc::new(Cell::new(Bounds::default())),
            goto: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            show_goto: false,
            show_presets: false,
            preset_index: 0,
            presets_search,
            last_handled_presets_search_version,
            filtered_presets,
            presets_focus,
            presets_scroll: ScrollHandle::new(),
            presets_trigger_bounds: Rc::new(Cell::new(Bounds::default())),
            focus,
            results_focus,
            active_pane: Pane::Main,
            drag_selecting: None,
            text_drag: None,
            main_row_layouts: RowLayoutCache::new(),
            results_row_layouts: RowLayoutCache::new(),
            render_frame: 0,
            scrollbar_drag: None,
            results_height,
            target_results_height: results_height,
            zoom_percent,
            results_resize_drag: None,
            resize_motion: ResizeMotion::default(),
            resize_frame_scheduled: false,
            main_vscroll_state: ScrollbarTrackState::new(),
            main_hscroll_state: ScrollbarTrackState::new(),
            results_vscroll_state: ScrollbarTrackState::new(),
            results_hscroll_state: ScrollbarTrackState::new(),
            sender,
            receiver,
            last_poll: Instant::now(),
            status: None,
            _timer: timer,
            _subscriptions: subscriptions,
            state_path: if session_error.is_none() {
                state_path
            } else {
                None
            },
            recent: saved.recent,
            show_files: false,
            header_click_armed: false,
            #[cfg(test)]
            header_zoom_count: 0,
        };
        for path in paths {
            this.open(path, cx);
        }
        if restore {
            for (tab, saved) in this.tabs.iter_mut().zip(saved.tabs) {
                tab.query = saved.query;
                tab.options = saved.options;
                tab.sync_highlight_regex();
                tab.bookmarks = saved.bookmarks;
                tab.selected = saved.selected;
                tab.main_selection = LineSelection::single(saved.selected);
                tab.follow = saved.follow;
            }
            this.active = saved.active.min(this.tabs.len().saturating_sub(1));
            let query = this
                .tabs
                .get(this.active)
                .map(|tab| tab.query.clone())
                .unwrap_or_default();
            this.input.update(cx, |input, cx| input.emplace(&query, cx));
        }
        this.sync_path_input(cx);
        if let Some(error) = session_error {
            this.status = Some(StatusMessage::error(format!(
                "{error} · session saving disabled"
            )));
        }
        this
    }

    fn save_session(&self, cx: &App) {
        if !self.config.files.save_session {
            return;
        }
        let Some(path) = &self.state_path else {
            return;
        };
        let session = Session {
            version: 1,
            active: self.active,
            recent: self.recent.clone(),
            results_height: Some(self.target_results_height.round()),
            zoom_percent: (self.zoom_percent != DEFAULT_ZOOM_PERCENT).then_some(self.zoom_percent),
            tabs: self
                .tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| SavedTab {
                    path: tab.path.clone(),
                    query: if index == self.active {
                        self.input.read(cx).as_str().to_owned()
                    } else {
                        tab.query.clone()
                    },
                    options: tab.options.clone(),
                    bookmarks: tab.bookmarks.clone(),
                    selected: tab.selected,
                    follow: tab.follow,
                })
                .collect(),
        };
        if let Err(error) = session.save(path) {
            eprintln!("Cannot save session {}: {error}", path.display());
        }
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = path.canonicalize().unwrap_or(path);
        if let Some(index) = self.tabs.iter().position(|tab| tab.path == path) {
            self.switch(index, cx);
            return;
        }
        self.recent.retain(|recent| recent != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(self.config.files.recent_limit);
        self.show_files = false;
        self.next_id += 1;
        self.tabs.push(Tab {
            id: self.next_id,
            path,
            log: None,
            loading: false,
            searching: false,
            searching_since: None,
            searching_visible: false,
            load_cancel: Arc::new(AtomicBool::new(false)),
            search_cancel: Arc::new(AtomicBool::new(false)),
            revision: 0,
            query: String::new(),
            options: self.config.search.clone(),
            highlight_regex: None,
            results: SearchResults::default(),
            bookmarks: BTreeSet::new(),
            selected: 0,
            main_selection: LineSelection::single(0),
            result_selection: LineSelection::empty(),
            main_text_selection: None,
            result_text_selection: None,
            follow: self.config.viewer.follow,
            scroll: UniformListScrollHandle::new(),
            result_scroll: UniformListScrollHandle::new(),
            error: None,
        });
        self.switch(self.tabs.len() - 1, cx);
        self.load(self.active);
        cx.notify();
    }

    fn switch(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.query = self.input.read(cx).as_str().to_string();
        }
        self.active = index;
        self.header_click_armed = false;
        self.drag_selecting = None;
        self.text_drag = None;
        self.scrollbar_drag = None;
        self.results_resize_drag = None;
        self.resize_motion.cancel();
        self.results_height = self.target_results_height;
        self.clear_scrollbar_states();
        self.clear_row_layouts();
        self.dismiss_presets(cx);
        self.close_path_completions();
        let query = self
            .tabs
            .get(index)
            .map(|tab| tab.query.clone())
            .unwrap_or_default();
        self.input.update(cx, |input, cx| input.emplace(&query, cx));
        self.sync_path_input(cx);
        cx.notify();
    }

    fn close_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if index != self.active
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.query = self.input.read(cx).as_str().to_string();
        }
        self.header_click_armed = false;
        self.drag_selecting = None;
        self.text_drag = None;
        self.scrollbar_drag = None;
        self.results_resize_drag = None;
        self.resize_motion.cancel();
        self.results_height = self.target_results_height;
        self.clear_scrollbar_states();
        self.clear_row_layouts();
        self.dismiss_presets(cx);
        self.close_path_completions();
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
            self.active_pane = Pane::Main;
            self.show_goto = false;
            self.input.update(cx, |input, cx| input.emplace("", cx));
        } else {
            if index < self.active {
                self.active -= 1;
            } else if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            }
            let query = self
                .tabs
                .get(self.active)
                .map(|tab| tab.query.clone())
                .unwrap_or_default();
            self.input.update(cx, |input, cx| input.emplace(&query, cx));
        }
        self.sync_path_input(cx);
        cx.notify();
    }

    fn replace_active_tab(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            self.open(path, cx);
            return;
        }
        let path = path.canonicalize().unwrap_or(path);
        self.recent.retain(|recent| recent != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(self.config.files.recent_limit);
        self.show_files = false;
        self.dismiss_presets(cx);
        self.show_goto = false;
        self.close_path_completions();

        let active = self.active;
        if self.tabs[active].path == path {
            self.sync_path_input(cx);
            self.load(active);
            cx.notify();
            return;
        }

        let current_query = self.input.read(cx).as_str().to_string();
        self.next_id += 1;
        let new_id = self.next_id;

        self.drag_selecting = None;
        self.text_drag = None;
        self.scrollbar_drag = None;
        self.results_resize_drag = None;
        self.resize_motion.cancel();
        self.results_height = self.target_results_height;
        self.clear_scrollbar_states();
        self.clear_row_layouts();

        let tab = &mut self.tabs[active];
        tab.load_cancel.store(true, Ordering::Relaxed);
        tab.search_cancel.store(true, Ordering::Relaxed);
        tab.id = new_id;
        tab.path = path;
        tab.log = None;
        tab.loading = false;
        tab.searching = false;
        tab.searching_since = None;
        tab.searching_visible = false;
        tab.load_cancel = Arc::new(AtomicBool::new(false));
        tab.search_cancel = Arc::new(AtomicBool::new(false));
        tab.revision += 1;
        tab.query = current_query;
        tab.sync_highlight_regex();
        tab.results = SearchResults::default();
        tab.bookmarks.clear();
        tab.selected = 0;
        tab.main_selection = LineSelection::single(0);
        tab.result_selection = LineSelection::empty();
        tab.main_text_selection = None;
        tab.result_text_selection = None;
        tab.scroll = UniformListScrollHandle::new();
        tab.result_scroll = UniformListScrollHandle::new();
        tab.error = None;

        self.sync_path_input(cx);
        self.load(active);
        cx.notify();
    }

    fn load(&mut self, index: usize) {
        let tab = &mut self.tabs[index];
        if tab.loading {
            return;
        }
        tab.loading = true;
        tab.search_cancel.store(true, Ordering::Relaxed);
        tab.revision += 1;
        tab.searching = false;
        tab.searching_since = None;
        tab.searching_visible = false;
        let (id, path, cancel, sender) = (
            tab.id,
            tab.path.clone(),
            tab.load_cancel.clone(),
            self.sender.clone(),
        );
        std::thread::spawn(move || {
            let result = LogFile::open(&path, &cancel)
                .map(Arc::new)
                .map_err(|error| error.to_string());
            let _ = sender.send(Message::Loaded(id, result));
        });
    }

    fn start_search(&mut self, index: usize, cx: &mut Context<Self>) {
        self.start_search_preserving(index, false, cx);
    }

    fn start_search_preserving(
        &mut self,
        index: usize,
        preserve_selection: bool,
        cx: &mut Context<Self>,
    ) {
        let include_bookmarks = self.config.viewer.include_bookmarks;
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        if index == self.active {
            tab.query = self.input.read(cx).as_str().to_string();
        }
        tab.sync_highlight_regex();
        tab.search_cancel.store(true, Ordering::Relaxed);
        tab.search_cancel = Arc::new(AtomicBool::new(false));
        tab.revision += 1;
        let Some(log) = tab.log.clone() else {
            return;
        };
        tab.error = None;
        let marks = if include_bookmarks {
            tab.bookmarks.clone()
        } else {
            BTreeSet::new()
        };
        if tab.query.is_empty() {
            tab.searching = false;
            tab.searching_since = None;
            tab.searching_visible = false;
            let (visible, widest_row) =
                search::visible_rows(&log, &[], tab.options.context_lines, &marks);
            let results = SearchResults {
                visible,
                widest_row,
                ..Default::default()
            };
            if preserve_selection {
                tab.result_selection
                    .remap_sorted(&tab.results.visible, &results.visible);
                if let Some(sel) = &mut tab.result_text_selection
                    && !sel.remap_sorted(&tab.results.visible, &results.visible)
                {
                    tab.result_text_selection = None;
                }
                tab.results = results;
            } else {
                let len = results.visible.len();
                tab.results = results;
                tab.result_selection = LineSelection::single_clamped(0, len);
                tab.result_text_selection = None;
                tab.result_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            cx.notify();
            return;
        }
        tab.searching = true;
        tab.searching_since = Some(Instant::now());
        tab.searching_visible = false;
        let (id, revision, query, options, cancel) = (
            tab.id,
            tab.revision,
            tab.query.clone(),
            tab.options.clone(),
            tab.search_cancel.clone(),
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    search::search(&log, &query, &options, &marks, &cancel)
                        .map_err(|error| error.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_search_result(id, revision, preserve_selection, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_search_result(
        &mut self,
        id: usize,
        revision: usize,
        preserve_selection: bool,
        result: Result<SearchResults, String>,
        cx: &mut Context<Self>,
    ) {
        let include_bookmarks = self.config.viewer.include_bookmarks;
        let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id && tab.revision == revision)
        else {
            return;
        };
        tab.searching = false;
        tab.searching_since = None;
        tab.searching_visible = false;
        match result {
            Ok(mut results) => {
                tab.sync_highlight_regex();
                if include_bookmarks && let Some(log) = &tab.log {
                    let (visible, widest_row) = search::visible_rows(
                        log,
                        &results.matches,
                        tab.options.context_lines,
                        &tab.bookmarks,
                    );
                    results.visible = visible;
                    results.widest_row = widest_row;
                }
                if preserve_selection {
                    tab.result_selection
                        .remap_sorted(&tab.results.visible, &results.visible);
                    if let Some(sel) = &mut tab.result_text_selection
                        && !sel.remap_sorted(&tab.results.visible, &results.visible)
                    {
                        tab.result_text_selection = None;
                    }
                    tab.results = results;
                } else {
                    let len = results.visible.len();
                    tab.results = results;
                    tab.result_selection = LineSelection::single_clamped(0, len);
                    tab.result_text_selection = None;
                    tab.result_scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
            }
            Err(error) => {
                tab.error = Some(error);
                tab.highlight_regex = None;
                tab.results = SearchResults::default();
                tab.result_selection = LineSelection::empty();
                tab.result_text_selection = None;
            }
        }
        cx.notify();
    }

    fn refresh_bookmarks(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.config.viewer.include_bookmarks {
            return;
        }
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let Some(log) = &tab.log else {
            return;
        };
        if tab.error.is_some() && !tab.query.is_empty() {
            return;
        }
        let (visible, widest_row) = search::visible_rows(
            log,
            &tab.results.matches,
            tab.options.context_lines,
            &tab.bookmarks,
        );
        tab.result_selection
            .remap_sorted(&tab.results.visible, &visible);
        if let Some(sel) = &mut tab.result_text_selection
            && !sel.remap_sorted(&tab.results.visible, &visible)
        {
            tab.result_text_selection = None;
        }
        tab.results.visible = visible;
        tab.results.widest_row = widest_row;
        cx.notify();
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.emplace("", cx));
        self.start_search(self.active, cx);
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(message) = self.receiver.try_recv() {
            changed = true;
            match message {
                Message::Loaded(id, result) => {
                    if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                        let tab = &mut self.tabs[index];
                        tab.loading = false;
                        match result {
                            Ok(log) => {
                                let initial = tab.log.is_none();
                                let replaced =
                                    tab.log.as_ref().is_some_and(|old| old.replaced_by(&log));
                                if replaced {
                                    tab.bookmarks.clear();
                                    tab.main_selection =
                                        LineSelection::single_clamped(0, log.len());
                                    tab.main_text_selection = None;
                                    tab.result_text_selection = None;
                                } else if initial || tab.main_selection.is_empty() {
                                    tab.main_selection =
                                        LineSelection::single_clamped(tab.selected, log.len());
                                    tab.main_text_selection = None;
                                } else {
                                    tab.main_selection.clamp(log.len());
                                    if let Some(sel) = &mut tab.main_text_selection
                                        && !sel.clamp_rows(log.len())
                                    {
                                        tab.main_text_selection = None;
                                    }
                                }
                                tab.selected = tab.main_selection.cursor;
                                tab.bookmarks.retain(|&line| line < log.len());
                                if initial && !tab.follow && !log.is_empty() {
                                    tab.scroll
                                        .scroll_to_item(tab.selected, ScrollStrategy::Center);
                                }
                                tab.log = Some(log);
                                tab.error = None;
                                if tab.follow {
                                    tab.scroll.scroll_to_bottom();
                                }
                                self.start_search_preserving(index, !replaced && !initial, cx);
                            }
                            Err(error) => {
                                tab.error = Some(error);
                            }
                        }
                    }
                }
                Message::Exported(result) => {
                    self.status = Some(match result {
                        Ok(()) => StatusMessage::success("Exported"),
                        Err(error) => StatusMessage::error(format!("Export failed: {error}")),
                    });
                }
            }
        }
        for tab in &mut self.tabs {
            if tab.searching
                && !tab.searching_visible
                && tab
                    .searching_since
                    .is_some_and(|since| since.elapsed() >= SEARCH_INDICATOR_DELAY)
            {
                tab.searching_visible = true;
                changed = true;
            }
        }
        if self.last_poll.elapsed() >= Duration::from_millis(self.config.viewer.poll_interval_ms) {
            self.last_poll = Instant::now();
            for index in 0..self.tabs.len() {
                let tab = &self.tabs[index];
                if !tab.loading
                    && !tab.searching
                    && let Some(log) = &tab.log
                {
                    match Stamp::read(&tab.path) {
                        Ok(stamp) if stamp != log.stamp => {
                            self.load(index);
                            changed = true;
                        }
                        Err(error) => {
                            self.tabs[index].error = Some(format!("Watching file: {error}"));
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn choose_file(&mut self, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open log files".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = picker.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(Some(paths))) => {
                    for path in paths {
                        this.open(path, cx);
                    }
                }
                Ok(Ok(None)) => {}
                other => {
                    this.status = Some(StatusMessage::error(format!(
                        "File dialog failed: {other:?}"
                    )));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(log) = tab.log.clone() else {
            return;
        };
        let indices = tab.results.visible.clone();
        let picker = cx.prompt_for_new_path(
            log.path.parent().unwrap_or(Path::new(".")),
            Some("nox-results.log"),
        );
        let sender = self.sender.clone();
        cx.spawn(async move |this, cx| match picker.await {
            Ok(Ok(Some(path))) => {
                std::thread::spawn(move || {
                    let _ = sender.send(Message::Exported(
                        log.export(&indices, &path).map_err(|e| e.to_string()),
                    ));
                });
            }
            Ok(Ok(None)) => {}
            other => {
                let _ = this.update(cx, |this, cx| {
                    this.status = Some(StatusMessage::error(format!(
                        "Save dialog failed: {other:?}"
                    )));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn active_pane_for(&self, window: &Window) -> Pane {
        if self.results_focus.is_focused(window) {
            Pane::Results
        } else if self.focus.is_focused(window) {
            Pane::Main
        } else {
            self.active_pane
        }
    }

    fn focus_pane(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        self.active_pane = pane;
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.dismiss_path_editing(cx);
        match pane {
            Pane::Main => self.focus.focus(window, cx),
            Pane::Results => {
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.result_selection.is_empty()
                    && !tab.results.visible.is_empty()
                {
                    let row = tab
                        .results
                        .visible
                        .binary_search(&tab.selected)
                        .unwrap_or_else(|idx| idx.min(tab.results.visible.len() - 1));
                    tab.result_selection.select_single(row);
                    tab.result_scroll
                        .scroll_to_item(row, ScrollStrategy::Nearest);
                }
                self.results_focus.focus(window, cx);
            }
        }
        cx.notify();
    }

    fn focus_visible_pane(&mut self, preferred: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let (min_h, max_h) = self.results_height_bounds(window);
        let eff = self.results_height.clamp(min_h, max_h);
        let main_hidden = is_main_hidden(eff, max_h);
        let results_hidden = is_results_hidden(eff);
        let pane = match preferred {
            Pane::Main if main_hidden && !results_hidden => Pane::Results,
            Pane::Results if results_hidden && !main_hidden => Pane::Main,
            _ => preferred,
        };
        self.focus_pane(pane, window, cx);
    }

    fn selection_count(tab: &Tab, pane: Pane) -> usize {
        match pane {
            Pane::Main => {
                let len = tab.log.as_ref().map_or(0, |log| log.len());
                if len == 0 {
                    0
                } else {
                    tab.main_selection.count().min(len)
                }
            }
            Pane::Results => {
                let len = tab.results.visible.len();
                if len == 0 {
                    0
                } else {
                    tab.result_selection.count().min(len)
                }
            }
        }
    }

    fn selected_source_lines(tab: &Tab, pane: Pane) -> Box<dyn Iterator<Item = usize> + '_> {
        match pane {
            Pane::Main => {
                let len = tab.log.as_ref().map_or(0, |log| log.len());
                Box::new(
                    tab.main_selection
                        .iter()
                        .take_while(move |&line| line < len),
                )
            }
            Pane::Results => Box::new(
                tab.result_selection
                    .iter()
                    .filter_map(|row| tab.results.visible.get(row).copied()),
            ),
        }
    }

    fn selection_all_bookmarked(tab: &Tab, pane: Pane) -> bool {
        let count = Self::selection_count(tab, pane);
        if count == 0 || count > tab.bookmarks.len() {
            return false;
        }
        Self::selected_source_lines(tab, pane).all(|line| tab.bookmarks.contains(&line))
    }

    fn sync_main_to_result_cursor(tab: &mut Tab) {
        if let Some(&source_line) = tab.results.visible.get(tab.result_selection.cursor) {
            tab.selected = source_line;
            tab.main_selection.select_single(source_line);
            tab.main_text_selection = None;
            tab.follow = false;
            tab.scroll
                .scroll_to_item(source_line, ScrollStrategy::Center);
        }
    }

    fn handle_row_mouse_down(
        &mut self,
        pane: Pane,
        row: usize,
        modifiers: Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane = pane;
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.dismiss_path_editing(cx);
        self.text_drag = None;
        match pane {
            Pane::Main => self.focus.focus(window, cx),
            Pane::Results => self.results_focus.focus(window, cx),
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            *tab.text_selection_mut(pane) = None;
            match pane {
                Pane::Main => {
                    let len = tab.log.as_ref().map_or(0, |log| log.len());
                    if row < len {
                        if modifiers.shift {
                            tab.main_selection.extend_to(row);
                            self.drag_selecting = (!modifiers.secondary()).then_some(Pane::Main);
                        } else if modifiers.secondary() {
                            tab.main_selection.toggle(row);
                            self.drag_selecting = None;
                        } else {
                            tab.main_selection.select_single(row);
                            self.drag_selecting = Some(Pane::Main);
                        }
                        tab.selected = row;
                        tab.follow = false;
                    }
                }
                Pane::Results => {
                    if row < tab.results.visible.len() {
                        if modifiers.shift {
                            tab.result_selection.extend_to(row);
                            self.drag_selecting = (!modifiers.secondary()).then_some(Pane::Results);
                        } else if modifiers.secondary() {
                            tab.result_selection.toggle(row);
                            self.drag_selecting = None;
                        } else {
                            tab.result_selection.select_single(row);
                            self.drag_selecting = Some(Pane::Results);
                        }
                        Self::sync_main_to_result_cursor(tab);
                    }
                }
            }
        }
        cx.notify();
    }

    fn handle_text_mouse_down(
        &mut self,
        pane: Pane,
        press: TextPressEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if press.modifiers.secondary() {
            return false;
        }
        if press.modifiers.shift {
            let existing = self
                .tabs
                .get(self.active)
                .and_then(|tab| tab.text_selection(pane));
            let Some(existing) = existing else {
                return false;
            };
            self.active_pane = pane;
            self.show_goto = false;
            self.dismiss_presets(cx);
            self.dismiss_path_editing(cx);
            self.drag_selecting = None;
            self.scrollbar_drag = None;
            match pane {
                Pane::Main => self.focus.focus(window, cx),
                Pane::Results => self.results_focus.focus(window, cx),
            }
            if let Some(tab) = self.tabs.get_mut(self.active) {
                let head = TextPosition::new(press.row, press.closest_offset);
                let next =
                    (existing.anchor != head).then_some(TextSelection::new(existing.anchor, head));
                *tab.text_selection_mut(pane) = next;
                self.text_drag = Some(TextDrag {
                    pane,
                    granularity: TextDragGranularity::Char,
                    anchor_start: existing.anchor,
                    anchor_end: existing.anchor,
                });
            }
            cx.notify();
            return true;
        }

        self.active_pane = pane;
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.dismiss_path_editing(cx);
        self.drag_selecting = None;
        self.scrollbar_drag = None;
        match pane {
            Pane::Main => self.focus.focus(window, cx),
            Pane::Results => self.results_focus.focus(window, cx),
        }

        if let Some(tab) = self.tabs.get_mut(self.active) {
            let in_bounds = match pane {
                Pane::Main => {
                    let len = tab.log.as_ref().map_or(0, |log| log.len());
                    if press.row < len {
                        tab.main_selection.select_single(press.row);
                        tab.selected = press.row;
                        tab.follow = false;
                        true
                    } else {
                        false
                    }
                }
                Pane::Results => {
                    if press.row < tab.results.visible.len() {
                        tab.result_selection.select_single(press.row);
                        Self::sync_main_to_result_cursor(tab);
                        true
                    } else {
                        false
                    }
                }
            };
            if in_bounds {
                match press.click_count {
                    2 => {
                        let selectable = &press.text[..press.selectable_len];
                        let word = word_range_at(selectable, press.hit_offset);
                        let start = TextPosition::new(press.row, word.start);
                        let end = TextPosition::new(press.row, word.end);
                        *tab.text_selection_mut(pane) =
                            (start != end).then_some(TextSelection::new(start, end));
                        self.text_drag = Some(TextDrag {
                            pane,
                            granularity: TextDragGranularity::Word,
                            anchor_start: start,
                            anchor_end: end,
                        });
                    }
                    count if count >= 3 => {
                        let start = TextPosition::new(press.row, 0);
                        let end = TextPosition::new(press.row, press.selectable_len);
                        *tab.text_selection_mut(pane) =
                            (start != end).then_some(TextSelection::new(start, end));
                        self.text_drag = Some(TextDrag {
                            pane,
                            granularity: TextDragGranularity::Line,
                            anchor_start: start,
                            anchor_end: end,
                        });
                    }
                    _ => {
                        *tab.text_selection_mut(pane) = None;
                        let pos = TextPosition::new(press.row, press.closest_offset);
                        self.text_drag = Some(TextDrag {
                            pane,
                            granularity: TextDragGranularity::Char,
                            anchor_start: pos,
                            anchor_end: pos,
                        });
                    }
                }
            }
        }
        cx.notify();
        true
    }

    fn handle_text_drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.text_drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.text_drag = None;
            cx.notify();
            return;
        }
        let Some(target) = self.row_layouts(drag.pane).row_at_y(event.position.y) else {
            return;
        };
        let (anchor, head) = match drag.granularity {
            TextDragGranularity::Char => {
                let offset = target.closest_byte_offset(event.position.x);
                (drag.anchor_start, TextPosition::new(target.row, offset))
            }
            TextDragGranularity::Word => {
                let selectable = &target.text[..target.selectable_len];
                let hit = target.hit_byte_offset(event.position.x);
                let word = word_range_at(selectable, hit);
                let w_start = TextPosition::new(target.row, word.start);
                let w_end = TextPosition::new(target.row, word.end);
                if w_end >= drag.anchor_end {
                    (drag.anchor_start, w_end)
                } else if w_start < drag.anchor_start {
                    (drag.anchor_end, w_start)
                } else {
                    (drag.anchor_start, drag.anchor_end)
                }
            }
            TextDragGranularity::Line => {
                if target.row >= drag.anchor_start.row {
                    (
                        drag.anchor_start,
                        TextPosition::new(target.row, target.selectable_len),
                    )
                } else {
                    (drag.anchor_end, TextPosition::new(target.row, 0))
                }
            }
        };
        let next = (anchor != head).then_some(TextSelection::new(anchor, head));
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let slot = tab.text_selection_mut(drag.pane);
            if *slot != next {
                *slot = next;
                cx.notify();
            }
        }
    }

    fn extract_selected_text(&self, pane: Pane) -> Option<Result<(String, usize), StatusMessage>> {
        let tab = self.tabs.get(self.active)?;
        let log = tab.log.as_ref()?;
        let selection = tab.text_selection(pane)?;
        let (start, end) = selection.ordered();
        let max_display_chars = self.config.viewer.max_display_chars;
        let tab_width = self.config.viewer.tab_width;
        let mut extracted = String::new();
        let mut line_count = 0usize;
        for row in start.row..=end.row {
            let Some(line) = (match pane {
                Pane::Main => (row < log.len()).then_some(row),
                Pane::Results => tab.results.visible.get(row).copied(),
            }) else {
                break;
            };
            if line_count > 0 {
                extracted.push('\n');
            }
            let (display_text, selectable_len, _) = format_display_line(
                log,
                line,
                max_display_chars,
                tab_width,
                None,
                SearchMode::Literal,
            );
            let selectable = &display_text[..selectable_len];
            if let Some(range) = selection.byte_range_for_row(row, selectable) {
                extracted.push_str(&selectable[range]);
            }
            line_count += 1;
            if extracted.len() as u64 > MAX_COPY_BYTES {
                return Some(Err(StatusMessage::warning(
                    "Selection too large to copy (limit 64 MB)",
                )));
            }
        }
        Some(Ok((extracted, line_count)))
    }

    fn copy_selected_text(&mut self, pane: Pane, cx: &mut Context<Self>) {
        let Some(result) = self.extract_selected_text(pane) else {
            return;
        };
        let (copied, line_count) = match result {
            Ok(extracted) => extracted,
            Err(warning) => {
                self.status = Some(warning);
                return;
            }
        };
        let char_count = copied.chars().count();
        cx.write_to_clipboard(ClipboardItem::new_string(copied));
        self.status = Some(StatusMessage::success(if line_count > 1 {
            format!(
                "Copied {} {} ({} lines)",
                format_count(char_count),
                if char_count == 1 { "char" } else { "chars" },
                format_count(line_count)
            )
        } else {
            format!(
                "Copied {} {}",
                format_count(char_count),
                if char_count == 1 { "char" } else { "chars" }
            )
        }));
    }

    fn search_selected_text(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.extract_selected_text(pane) else {
            return;
        };
        let selected = match result {
            Ok((text, _)) => sanitize_single_line_search_text(&text),
            Err(warning) => {
                self.status = Some(warning);
                return;
            }
        };
        if selected.is_empty() {
            return;
        }
        let regex_mode = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.options.mode == SearchMode::Regex);
        let text = if regex_mode {
            regex::escape(&selected)
        } else {
            selected
        };
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.input.update(cx, |input, cx| {
            input.emplace(&text, cx);
            input.move_to(text.len(), cx);
        });
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn append_selected_text_to_search(
        &mut self,
        pane: Pane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.options.mode != SearchMode::Regex {
            return;
        }
        let Some(result) = self.extract_selected_text(pane) else {
            return;
        };
        let selected = match result {
            Ok((text, _)) => sanitize_single_line_search_text(&text),
            Err(warning) => {
                self.status = Some(warning);
                return;
            }
        };
        if selected.is_empty() {
            return;
        }
        let escaped = regex::escape(&selected);
        let next = append_regex_alternative(self.input.read(cx).as_str(), &escaped);
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.input.update(cx, |input, cx| {
            input.emplace(&next, cx);
            input.move_to(next.len(), cx);
        });
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn selected_text_label(&self, tab: &Tab, pane: Pane) -> Option<String> {
        let selection = tab.text_selection(pane)?;
        let log = tab.log.as_ref()?;
        let (start, end) = selection.ordered();
        let rows = end.row - start.row + 1;
        const MAX_INLINE_CHAR_COUNT_ROWS: usize = 64;
        if rows > MAX_INLINE_CHAR_COUNT_ROWS {
            return Some(format!("{} lines selected", format_count(rows)));
        }
        let max_display_chars = self.config.viewer.max_display_chars;
        let tab_width = self.config.viewer.tab_width;
        let mut chars = 0usize;
        for row in start.row..=end.row {
            let line = match pane {
                Pane::Main => (row < log.len()).then_some(row)?,
                Pane::Results => *tab.results.visible.get(row)?,
            };
            if row > start.row {
                chars += 1;
            }
            let (display_text, selectable_len, _) = format_display_line(
                log,
                line,
                max_display_chars,
                tab_width,
                None,
                SearchMode::Literal,
            );
            let selectable = &display_text[..selectable_len];
            if let Some(range) = selection.byte_range_for_row(row, selectable) {
                chars += selectable[range].chars().count();
            }
        }
        Some(format!(
            "{} {} selected",
            format_count(chars),
            if chars == 1 { "char" } else { "chars" }
        ))
    }

    fn handle_row_mouse_move(
        &mut self,
        pane: Pane,
        row: usize,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        if self.drag_selecting != Some(pane) {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag_selecting = None;
            return;
        }
        let mut changed = false;
        if let Some(tab) = self.tabs.get_mut(self.active) {
            match pane {
                Pane::Main => {
                    let len = tab.log.as_ref().map_or(0, |log| log.len());
                    if row < len && tab.main_selection.cursor != row {
                        tab.main_selection.extend_to(row);
                        tab.selected = row;
                        tab.follow = false;
                        changed = true;
                    }
                }
                Pane::Results => {
                    if row < tab.results.visible.len() && tab.result_selection.cursor != row {
                        tab.result_selection.extend_to(row);
                        Self::sync_main_to_result_cursor(tab);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn toggle_bookmark_line(&mut self, line: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active)
            && tab.log.as_ref().is_some_and(|log| line < log.len())
        {
            if !tab.bookmarks.remove(&line) {
                tab.bookmarks.insert(line);
            }
            self.refresh_bookmarks(self.active, cx);
        }
        cx.notify();
    }

    fn navigate(
        &mut self,
        command: Command,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_for(window);
        self.active_pane = pane;
        let results_h = self.effective_results_height(window);
        let line_height = self.zoom_metrics().line_height;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        match pane {
            Pane::Main => {
                let Some(log) = &tab.log else {
                    return;
                };
                let len = log.len();
                if len == 0 {
                    return;
                }
                tab.main_text_selection = None;
                let step = ((f32::from(window.viewport_size().height) - results_h - 180.)
                    / line_height)
                    .max(1.) as usize;
                let current = tab.main_selection.cursor.min(len - 1);
                let target = match command {
                    Command::NextLine | Command::SelectNextLine => current.saturating_add(1),
                    Command::PreviousLine | Command::SelectPreviousLine => {
                        current.saturating_sub(1)
                    }
                    Command::FirstLine | Command::SelectFirstLine => 0,
                    Command::LastLine | Command::SelectLastLine => len - 1,
                    Command::PageUp | Command::SelectPageUp => current.saturating_sub(step),
                    _ => current.saturating_add(step),
                }
                .min(len - 1);
                if extend {
                    tab.main_selection.extend_to(target);
                } else {
                    tab.main_selection.select_single(target);
                }
                tab.selected = target;
                tab.follow = false;
                tab.scroll.scroll_to_item(target, ScrollStrategy::Nearest);
            }
            Pane::Results => {
                let len = tab.results.visible.len();
                if len == 0 {
                    return;
                }
                tab.result_text_selection = None;
                let step = (results_h / line_height).max(1.) as usize;
                let current = tab.result_selection.cursor.min(len - 1);
                let target = match command {
                    Command::NextLine | Command::SelectNextLine => current.saturating_add(1),
                    Command::PreviousLine | Command::SelectPreviousLine => {
                        current.saturating_sub(1)
                    }
                    Command::FirstLine | Command::SelectFirstLine => 0,
                    Command::LastLine | Command::SelectLastLine => len - 1,
                    Command::PageUp | Command::SelectPageUp => current.saturating_sub(step),
                    _ => current.saturating_add(step),
                }
                .min(len - 1);
                if extend {
                    tab.result_selection.extend_to(target);
                } else {
                    tab.result_selection.select_single(target);
                }
                tab.result_scroll
                    .scroll_to_item(target, ScrollStrategy::Nearest);
                Self::sync_main_to_result_cursor(tab);
            }
        }
        cx.notify();
    }

    fn command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::ShowFiles => {
                self.dismiss_presets(cx);
                self.show_files = !self.show_files;
            }
            Command::TogglePresets => {
                self.toggle_presets(window, cx);
                return;
            }
            Command::ApplyPreset => {
                self.apply_selected_preset(window, cx);
                return;
            }
            Command::NextLine
            | Command::PreviousLine
            | Command::FirstLine
            | Command::LastLine
            | Command::PageUp
            | Command::PageDown => {
                if self.path_input_focused(window, cx)
                    && matches!(command, Command::NextLine | Command::PreviousLine)
                {
                    self.navigate_path_completions(command == Command::NextLine, cx);
                    return;
                }
                if self.show_presets
                    && self.presets_focused(window, cx)
                    && matches!(
                        command,
                        Command::NextLine
                            | Command::PreviousLine
                            | Command::FirstLine
                            | Command::LastLine
                    )
                {
                    self.navigate_presets(command, cx);
                    return;
                }
                self.navigate(command, false, window, cx);
                return;
            }
            Command::SelectNextLine
            | Command::SelectPreviousLine
            | Command::SelectFirstLine
            | Command::SelectLastLine
            | Command::SelectPageUp
            | Command::SelectPageDown => {
                self.navigate(command, true, window, cx);
                return;
            }
            Command::SelectAll => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    *tab.text_selection_mut(pane) = None;
                    match pane {
                        Pane::Main => {
                            let len = tab.log.as_ref().map_or(0, |log| log.len());
                            tab.main_selection.select_all(len);
                            if len > 0 {
                                tab.selected = tab.main_selection.cursor;
                            }
                        }
                        Pane::Results => {
                            let len = tab.results.visible.len();
                            tab.result_selection.select_all(len);
                        }
                    }
                }
            }
            Command::ClearTextSelection => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                self.text_drag = None;
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    *tab.text_selection_mut(pane) = None;
                }
            }
            Command::ClearSelection => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                self.text_drag = None;
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    *tab.text_selection_mut(pane) = None;
                    match pane {
                        Pane::Main => {
                            let len = tab.log.as_ref().map_or(0, |log| log.len());
                            tab.main_selection.collapse_to_cursor(len);
                            if len > 0 {
                                tab.selected = tab.main_selection.cursor;
                            }
                        }
                        Pane::Results => {
                            let len = tab.results.visible.len();
                            tab.result_selection.collapse_to_cursor(len);
                            Self::sync_main_to_result_cursor(tab);
                        }
                    }
                }
            }
            Command::SwapSelectionAnchor => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    match pane {
                        Pane::Main => {
                            let len = tab.log.as_ref().map_or(0, |log| log.len());
                            if len > 0 && tab.main_selection.swap_anchor() {
                                let cursor = tab.main_selection.cursor.min(len - 1);
                                tab.main_selection.cursor = cursor;
                                tab.selected = cursor;
                                tab.follow = false;
                                tab.scroll.scroll_to_item(cursor, ScrollStrategy::Nearest);
                            }
                        }
                        Pane::Results => {
                            let len = tab.results.visible.len();
                            if len > 0 && tab.result_selection.swap_anchor() {
                                let cursor = tab.result_selection.cursor.min(len - 1);
                                tab.result_selection.cursor = cursor;
                                tab.result_scroll
                                    .scroll_to_item(cursor, ScrollStrategy::Nearest);
                                Self::sync_main_to_result_cursor(tab);
                            }
                        }
                    }
                }
            }
            Command::SwitchPane => {
                let current = self.active_pane_for(window);
                let (min_h, max_h) = self.results_height_bounds(window);
                let eff = self.results_height.clamp(min_h, max_h);
                match current {
                    Pane::Main => {
                        let has_results = self
                            .tabs
                            .get(self.active)
                            .is_some_and(|tab| !tab.results.visible.is_empty());
                        if has_results && !is_results_hidden(eff) {
                            self.focus_pane(Pane::Results, window, cx);
                            return;
                        }
                    }
                    Pane::Results => {
                        if !is_main_hidden(eff, max_h) {
                            self.focus_pane(Pane::Main, window, cx);
                            return;
                        }
                    }
                }
            }
            Command::NextBookmark | Command::PreviousBookmark => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    if pane == Pane::Results && !tab.results.visible.is_empty() {
                        let current_row = tab
                            .result_selection
                            .cursor
                            .min(tab.results.visible.len() - 1);
                        let target_row = if command == Command::NextBookmark {
                            ((current_row + 1)..tab.results.visible.len())
                                .chain(0..=current_row)
                                .find(|&r| tab.bookmarks.contains(&tab.results.visible[r]))
                        } else {
                            (0..current_row)
                                .rev()
                                .chain((current_row..tab.results.visible.len()).rev())
                                .find(|&r| tab.bookmarks.contains(&tab.results.visible[r]))
                        };
                        if let Some(row) = target_row {
                            tab.result_selection.select_single(row);
                            tab.result_text_selection = None;
                            tab.result_scroll
                                .scroll_to_item(row, ScrollStrategy::Center);
                            Self::sync_main_to_result_cursor(tab);
                        }
                    } else {
                        let target = if command == Command::NextBookmark {
                            tab.bookmarks
                                .iter()
                                .find(|&&line| line > tab.selected)
                                .or(tab.bookmarks.first())
                        } else {
                            tab.bookmarks
                                .iter()
                                .rev()
                                .find(|&&line| line < tab.selected)
                                .or(tab.bookmarks.last())
                        };
                        if let Some(&line) = target {
                            tab.selected = line;
                            tab.main_selection.select_single(line);
                            tab.main_text_selection = None;
                            tab.follow = false;
                            tab.scroll.scroll_to_item(line, ScrollStrategy::Center);
                            if let Ok(index) = tab.results.visible.binary_search(&line) {
                                tab.result_selection.select_single(index);
                                tab.result_text_selection = None;
                                tab.result_scroll
                                    .scroll_to_item(index, ScrollStrategy::Center);
                            }
                        }
                    }
                }
            }
            Command::Open => self.choose_file(cx),
            Command::Quit => cx.quit(),
            Command::FocusSearch => {
                self.show_goto = false;
                self.dismiss_presets(cx);
                self.dismiss_path_editing(cx);
                self.input.read(cx).focus_handle(cx).focus(window, cx);
            }
            Command::FocusFilePath => {
                self.focus_file_path(window, cx);
                return;
            }
            Command::ReplaceLogPath => {
                self.submit_file_path(false, window, cx);
                return;
            }
            Command::OpenPathInNewTab => {
                self.submit_file_path(true, window, cx);
                return;
            }
            Command::CompleteFilePath => {
                self.complete_file_path(window, cx);
                return;
            }
            Command::FocusLog => {
                if self.show_presets {
                    self.close_presets(window, cx);
                } else if self.path_input_focused(window, cx) && self.show_path_completions {
                    self.close_path_completions();
                    cx.notify();
                } else {
                    if self.path_input_focused(window, cx) {
                        self.sync_path_input(cx);
                    }
                    self.focus_visible_pane(Pane::Main, window, cx);
                }
                return;
            }
            Command::Search => self.start_search(self.active, cx),
            Command::ToggleRegex
            | Command::ToggleCaseSensitive
            | Command::ToggleWholeWord
            | Command::ToggleInvert => {
                let input_empty = self.input.read(cx).as_str().is_empty();
                let should_search = if let Some(tab) = self.tabs.get_mut(self.active) {
                    match command {
                        Command::ToggleRegex => {
                            tab.options.mode = if tab.options.mode == SearchMode::Regex {
                                SearchMode::Literal
                            } else {
                                SearchMode::Regex
                            };
                        }
                        Command::ToggleCaseSensitive => {
                            tab.options.case_sensitive = !tab.options.case_sensitive;
                        }
                        Command::ToggleWholeWord => {
                            tab.options.whole_word = !tab.options.whole_word;
                        }
                        Command::ToggleInvert => {
                            tab.options.invert = !tab.options.invert;
                        }
                        _ => unreachable!(),
                    }
                    !input_empty || !tab.query.is_empty()
                } else {
                    false
                };
                if should_search {
                    self.start_search(self.active, cx);
                }
            }
            Command::ToggleHighlightMatches => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.options.highlight_matches = !tab.options.highlight_matches;
                }
            }
            Command::CloseTab => {
                let index = self.active;
                self.close_tab_at(index, cx);
            }
            Command::NextTab => {
                if !self.tabs.is_empty() {
                    self.switch((self.active + 1) % self.tabs.len(), cx);
                }
            }
            Command::PreviousTab => {
                if !self.tabs.is_empty() {
                    let len = self.tabs.len();
                    self.switch((self.active + len - 1) % len, cx);
                }
            }
            Command::ToggleFollow => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.follow = !tab.follow;
                    if tab.follow {
                        tab.scroll.scroll_to_bottom();
                    }
                }
            }
            Command::ToggleBookmark => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.log.as_ref().is_some_and(|log| !log.is_empty())
                {
                    let count = Self::selection_count(tab, pane);
                    if count == 0 {
                        return;
                    }
                    if count > MAX_BULK_BOOKMARK_LINES {
                        self.status = Some(StatusMessage::warning(format!(
                            "Too many lines selected to bookmark (limit {})",
                            format_count(MAX_BULK_BOOKMARK_LINES)
                        )));
                    } else {
                        let lines: Vec<usize> = Self::selected_source_lines(tab, pane).collect();
                        let all_marked = lines.iter().all(|line| tab.bookmarks.contains(line));
                        if all_marked {
                            for line in lines {
                                tab.bookmarks.remove(&line);
                            }
                        } else {
                            for line in lines {
                                tab.bookmarks.insert(line);
                            }
                        }
                        self.refresh_bookmarks(self.active, cx);
                    }
                }
            }
            Command::NextMatch | Command::PreviousMatch => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let matches = &tab.results.matches;
                    let target = if command == Command::NextMatch {
                        matches
                            .iter()
                            .find(|&&line| line > tab.selected)
                            .or(matches.first())
                    } else {
                        matches
                            .iter()
                            .rev()
                            .find(|&&line| line < tab.selected)
                            .or(matches.last())
                    };
                    if let Some(&target) = target {
                        tab.selected = target;
                        tab.main_selection.select_single(target);
                        tab.main_text_selection = None;
                        tab.follow = false;
                        tab.scroll.scroll_to_item(target, ScrollStrategy::Center);
                        if let Ok(index) = tab.results.visible.binary_search(&target) {
                            tab.result_selection.select_single(index);
                            tab.result_text_selection = None;
                            tab.result_scroll
                                .scroll_to_item(index, ScrollStrategy::Center);
                        }
                    }
                }
            }
            Command::CopySelectedText => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                self.copy_selected_text(pane, cx);
            }
            Command::SearchSelectedText => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                self.search_selected_text(pane, window, cx);
            }
            Command::AppendSelectedTextToSearch => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                self.append_selected_text_to_search(pane, window, cx);
            }
            Command::CopyLine => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                if let Some(tab) = self.tabs.get(self.active)
                    && let Some(log) = &tab.log
                {
                    let count = Self::selection_count(tab, pane);
                    if count == 0 {
                        self.status = Some(StatusMessage::warning("No lines selected"));
                    } else {
                        let mut total_bytes: u64 = 0;
                        let mut lines_to_copy = Vec::with_capacity(count.min(1024));
                        let mut exceeded = false;
                        for line in Self::selected_source_lines(tab, pane) {
                            total_bytes = total_bytes.saturating_add(log.line_bytes(line));
                            if total_bytes > MAX_COPY_BYTES {
                                exceeded = true;
                                break;
                            }
                            lines_to_copy.push(line);
                        }
                        if exceeded {
                            self.status = Some(StatusMessage::warning(
                                "Selection too large to copy (limit 64 MB)",
                            ));
                        } else if lines_to_copy.is_empty() {
                            self.status = Some(StatusMessage::warning("No lines selected"));
                        } else {
                            let mut copied = String::with_capacity(total_bytes as usize);
                            let mut read_error = None;
                            for (index, &line) in lines_to_copy.iter().enumerate() {
                                if index > 0 {
                                    copied.push('\n');
                                }
                                match log.line(line, usize::MAX) {
                                    Ok(text) => copied.push_str(&text),
                                    Err(error) => {
                                        read_error = Some(error.to_string());
                                        break;
                                    }
                                }
                            }
                            if let Some(error) = read_error {
                                self.status = Some(StatusMessage::error(error));
                            } else {
                                cx.write_to_clipboard(ClipboardItem::new_string(copied));
                                self.status =
                                    Some(StatusMessage::success(if lines_to_copy.len() == 1 {
                                        format!("Copied Ln {}", format_count(lines_to_copy[0] + 1))
                                    } else {
                                        format!(
                                            "Copied {} lines",
                                            format_count(lines_to_copy.len())
                                        )
                                    }));
                            }
                        }
                    }
                }
            }
            Command::Reload => {
                if !self.tabs.is_empty() {
                    self.load(self.active);
                }
            }
            Command::GoToLine => {
                self.dismiss_presets(cx);
                if self.tabs.is_empty() {
                    self.show_goto = false;
                } else if self.show_goto {
                    let line = self.goto.read(cx).as_str().trim().parse::<usize>();
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        if let (Ok(line), Some(log)) = (line, &tab.log) {
                            if line > 0 && line <= log.len() {
                                tab.selected = line - 1;
                                tab.main_selection.select_single(line - 1);
                                tab.main_text_selection = None;
                                tab.follow = false;
                                tab.scroll.scroll_to_item(line - 1, ScrollStrategy::Center);
                                self.show_goto = false;
                                self.status = None;
                                self.active_pane = Pane::Main;
                                self.focus.focus(window, cx);
                            } else {
                                self.status = Some(StatusMessage::warning(format!(
                                    "Range: 1–{}",
                                    format_count(log.len())
                                )));
                            }
                        } else {
                            self.status = Some(StatusMessage::warning("Invalid line number"));
                        }
                    }
                } else {
                    let (min_h, max_h) = self.results_height_bounds(window);
                    let eff = self.results_height.clamp(min_h, max_h);
                    if is_main_hidden(eff, max_h) {
                        let restored = self
                            .config
                            .window
                            .results_height
                            .min((max_h - 140.0).max(0.0))
                            .clamp(min_h, max_h);
                        self.resize_motion.cancel();
                        self.results_height = restored;
                        self.target_results_height = restored;
                    }
                    self.show_goto = true;
                    self.dismiss_path_editing(cx);
                    self.goto.update(cx, |input, cx| input.emplace("", cx));
                    self.goto.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
            Command::ExportResults => self.export(cx),
            Command::IncreaseFilterHeight => {
                self.resize_filter_height_keyboard(ResizeDirection::Grow, window, cx);
                return;
            }
            Command::DecreaseFilterHeight => {
                self.resize_filter_height_keyboard(ResizeDirection::Shrink, window, cx);
                return;
            }
            Command::ZoomIn => {
                self.set_zoom_percent(zoom_in_percent(self.zoom_percent), cx);
                return;
            }
            Command::ZoomOut => {
                self.set_zoom_percent(zoom_out_percent(self.zoom_percent), cx);
                return;
            }
            Command::ZoomReset => {
                self.set_zoom_percent(DEFAULT_ZOOM_PERCENT, cx);
                return;
            }
        }
        cx.notify();
    }

    fn zoom_metrics(&self) -> ZoomMetrics {
        ZoomMetrics::compute(
            self.config.appearance.font_size,
            self.config.viewer.line_height,
            f32::from(self.theme.small_size),
            self.zoom_percent,
        )
    }

    fn scale_uniform_list_scroll(
        handle: &UniformListScrollHandle,
        old_metrics: ZoomMetrics,
        new_metrics: ZoomMetrics,
        row_count: usize,
    ) {
        let state = handle.0.borrow();
        let offset = state.base_handle.offset();
        let viewport_height = state
            .last_item_size
            .map(|item_size| f32::from(item_size.item.height));
        let (next_x, next_y) = scaled_scroll_offset(
            f32::from(offset.x),
            f32::from(offset.y),
            old_metrics.line_height,
            new_metrics.line_height,
            old_metrics.scale,
            new_metrics.scale,
            viewport_height,
            row_count,
        );
        state.base_handle.set_offset(point(px(next_x), px(next_y)));
    }

    fn set_zoom_percent(&mut self, next_percent: u16, cx: &mut Context<Self>) {
        let clamped = clamp_zoom_percent(next_percent);
        if clamped == self.zoom_percent {
            return;
        }
        let old_metrics = self.zoom_metrics();
        self.zoom_percent = clamped;
        let new_metrics = self.zoom_metrics();
        self.text_drag = None;
        self.scrollbar_drag = None;
        self.clear_row_layouts();
        for tab in &mut self.tabs {
            let main_rows = tab.log.as_ref().map_or(0, |log| log.len());
            Self::scale_uniform_list_scroll(&tab.scroll, old_metrics, new_metrics, main_rows);
            if tab.follow {
                tab.scroll.scroll_to_bottom();
            }
            let result_rows = tab.results.visible.len();
            Self::scale_uniform_list_scroll(
                &tab.result_scroll,
                old_metrics,
                new_metrics,
                result_rows,
            );
        }
        cx.notify();
    }

    fn toggle_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_goto {
            self.command(Command::FocusLog, window, cx);
        } else {
            self.command(Command::GoToLine, window, cx);
        }
    }

    fn path_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.path_input.read(cx).focus_handle(cx).is_focused(window)
    }

    fn set_path_input_silently(&mut self, text: &str, cx: &mut Context<Self>) {
        let version = self.path_input.update(cx, |input, cx| {
            if input.as_str() != text {
                input.emplace(text, cx);
            }
            input.version()
        });
        self.last_handled_path_version = version;
    }

    fn sync_path_input(&mut self, cx: &mut Context<Self>) {
        let path = self
            .tabs
            .get(self.active)
            .map(|tab| tab.path.display().to_string())
            .unwrap_or_default();
        self.set_path_input_silently(&path, cx);
    }

    fn close_path_completions(&mut self) {
        self.show_path_completions = false;
        self.path_completion_index = None;
        self.path_completions = CompletionResult::default();
    }

    fn dismiss_path_editing(&mut self, cx: &mut Context<Self>) {
        self.close_path_completions();
        self.sync_path_input(cx);
    }

    fn refresh_path_completions(&mut self, open_menu: bool, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).as_str().to_owned();
        self.path_completions = complete_path(&text);
        self.path_completion_index = None;
        self.show_path_completions = open_menu && !self.path_completions.items.is_empty();
        if self.show_path_completions {
            self.path_completions_scroll.scroll_to_item(0);
        }
        cx.notify();
    }

    fn on_path_input_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let version = self.path_input.read(cx).version();
        if version == self.last_handled_path_version {
            return;
        }
        self.last_handled_path_version = version;
        if !self.path_input_focused(window, cx) {
            return;
        }
        self.refresh_path_completions(true, cx);
    }

    fn on_path_input_blur(&mut self, cx: &mut Context<Self>) {
        self.dismiss_path_editing(cx);
        cx.notify();
    }

    fn focus_file_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.sync_path_input(cx);
        self.path_input.update(cx, |input, cx| {
            input.select_document(cx);
        });
        self.path_input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn navigate_path_completions(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !self.show_path_completions {
            self.refresh_path_completions(true, cx);
            if !self.path_completions.items.is_empty() {
                let idx = if forward {
                    0
                } else {
                    self.path_completions.items.len() - 1
                };
                self.path_completion_index = Some(idx);
                self.path_completions_scroll.scroll_to_item(idx);
                cx.notify();
            }
            return;
        }
        let len = self.path_completions.items.len();
        if len == 0 {
            self.path_completion_index = None;
            return;
        }
        let next = match (self.path_completion_index, forward) {
            (None, true) => 0,
            (None, false) => len - 1,
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
        };
        self.path_completion_index = Some(next);
        self.path_completions_scroll.scroll_to_item(next);
        cx.notify();
    }

    fn apply_path_completion_text(
        &mut self,
        item: &PathCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let replacement = &item.replacement;
        let version = self.path_input.update(cx, |input, cx| {
            input.emplace(replacement, cx);
            input.move_to(replacement.len(), cx);
            input.version()
        });
        self.last_handled_path_version = version;
        self.path_input.read(cx).focus_handle(cx).focus(window, cx);
        if item.is_dir {
            self.refresh_path_completions(true, cx);
        } else {
            self.close_path_completions();
            cx.notify();
        }
    }

    fn complete_file_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_path_completions
            && let Some(index) = self.path_completion_index
            && let Some(item) = self.path_completions.items.get(index).cloned()
        {
            self.apply_path_completion_text(&item, window, cx);
            return;
        }

        if !self.show_path_completions {
            let text = self.path_input.read(cx).as_str().to_owned();
            self.path_completions = complete_path(&text);
            self.path_completion_index = None;
        }

        if self.path_completions.items.is_empty() {
            self.show_path_completions = false;
            cx.notify();
            return;
        }

        if self.path_completions.items.len() == 1 {
            let item = self.path_completions.items[0].clone();
            self.apply_path_completion_text(&item, window, cx);
            return;
        }

        if let Some(prefix) = self.path_completions.common_prefix.clone() {
            let version = self.path_input.update(cx, |input, cx| {
                input.emplace(&prefix, cx);
                input.move_to(prefix.len(), cx);
                input.version()
            });
            self.last_handled_path_version = version;
            self.refresh_path_completions(true, cx);
            return;
        }

        self.show_path_completions = true;
        self.path_completion_index = Some(0);
        self.path_completions_scroll.scroll_to_item(0);
        cx.notify();
    }

    fn activate_path_completion(
        &mut self,
        index: usize,
        open_in_new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.path_completions.items.get(index).cloned() else {
            return;
        };
        if item.is_dir {
            self.apply_path_completion_text(&item, window, cx);
        } else {
            self.open_or_replace_from_path_bar(item.resolved, open_in_new_tab, window, cx);
        }
    }

    fn open_or_replace_from_path_bar(
        &mut self,
        path: PathBuf,
        open_in_new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = None;
        self.close_path_completions();
        if open_in_new_tab {
            self.open(path, cx);
        } else {
            self.replace_active_tab(path, cx);
        }
        self.focus_visible_pane(Pane::Main, window, cx);
    }

    fn submit_file_path(
        &mut self,
        open_in_new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.show_path_completions
            && let Some(index) = self.path_completion_index
        {
            self.activate_path_completion(index, open_in_new_tab, window, cx);
            return;
        }

        let raw = self.path_input.read(cx).as_str().to_owned();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = home_dir();
        let Some(resolved) = resolve_input_path(&raw, &cwd, home.as_deref()) else {
            self.status = Some(StatusMessage::warning("Enter a file path"));
            cx.notify();
            return;
        };

        match std::fs::metadata(&resolved) {
            Ok(meta) if meta.is_dir() => {
                let with_sep = ensure_trailing_separator(raw.trim());
                if raw != with_sep {
                    let version = self.path_input.update(cx, |input, cx| {
                        input.emplace(&with_sep, cx);
                        input.move_to(with_sep.len(), cx);
                        input.version()
                    });
                    self.last_handled_path_version = version;
                    self.refresh_path_completions(true, cx);
                } else {
                    self.refresh_path_completions(true, cx);
                    self.status = Some(StatusMessage::warning(
                        "Path is a directory — choose a file",
                    ));
                    cx.notify();
                }
            }
            Ok(meta) if !meta.is_file() => {
                self.status = Some(StatusMessage::error(format!(
                    "{} is not a regular file",
                    resolved.display()
                )));
                cx.notify();
            }
            Ok(_) => match std::fs::File::open(&resolved) {
                Ok(_) => {
                    self.open_or_replace_from_path_bar(resolved, open_in_new_tab, window, cx);
                }
                Err(error) => {
                    self.status = Some(StatusMessage::error(format!(
                        "Cannot open {}: {error}",
                        resolved.display()
                    )));
                    cx.notify();
                }
            },
            Err(error) => {
                self.status = Some(StatusMessage::error(format!(
                    "Cannot open {}: {error}",
                    resolved.display()
                )));
                cx.notify();
            }
        }
    }

    fn matching_preset_index(&self, current_input: &str, options: &SearchConfig) -> Option<usize> {
        self.config.search.presets.iter().position(|preset| {
            preset.pattern == current_input
                && preset.mode == options.mode
                && preset.highlight_match == options.highlight_matches
        })
    }

    fn active_preset_index(&self, cx: &App) -> Option<usize> {
        let current_input = self.input.read(cx).as_str();
        let options = self
            .tabs
            .get(self.active)
            .map_or(&self.config.search, |tab| &tab.options);
        self.matching_preset_index(current_input, options)
    }

    fn presets_search_focused(&self, window: &Window, cx: &App) -> bool {
        self.presets_search
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
    }

    fn presets_focused(&self, window: &Window, cx: &App) -> bool {
        self.presets_search_focused(window, cx) || self.presets_focus.is_focused(window)
    }

    fn set_presets_search_silently(&mut self, text: &str, cx: &mut Context<Self>) {
        let version = self.presets_search.update(cx, |input, cx| {
            if input.as_str() != text {
                input.emplace(text, cx);
            }
            input.version()
        });
        self.last_handled_presets_search_version = version;
    }

    fn refresh_filtered_presets(&mut self, select_active_when_empty: bool, cx: &mut Context<Self>) {
        let query = self.presets_search.read(cx).as_str().to_owned();
        self.filtered_presets = fuzzy::filter_presets(&self.config.search.presets, &query);
        if self.filtered_presets.is_empty() {
            self.preset_index = 0;
        } else if select_active_when_empty && query.trim().is_empty() {
            let active = self.active_preset_index(cx);
            self.preset_index = active
                .and_then(|idx| self.filtered_presets.iter().position(|m| m.index == idx))
                .unwrap_or(0);
            self.presets_scroll.scroll_to_item(self.preset_index);
        } else {
            self.preset_index = 0;
            self.presets_scroll.scroll_to_item(0);
        }
    }

    fn on_presets_search_changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let version = self.presets_search.read(cx).version();
        if version == self.last_handled_presets_search_version {
            return;
        }
        self.last_handled_presets_search_version = version;
        if !self.show_presets {
            return;
        }
        self.refresh_filtered_presets(false, cx);
        cx.notify();
    }

    fn dismiss_presets(&mut self, cx: &mut Context<Self>) {
        if !self.show_presets && self.presets_search.read(cx).as_str().is_empty() {
            return;
        }
        self.show_presets = false;
        self.set_presets_search_silently("", cx);
        self.filtered_presets = fuzzy::filter_presets(&self.config.search.presets, "");
    }

    fn clear_presets_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_presets_search_silently("", cx);
        self.refresh_filtered_presets(false, cx);
        self.presets_search
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    fn toggle_presets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() || self.config.search.presets.is_empty() {
            self.dismiss_presets(cx);
            return;
        }
        if self.show_presets {
            self.close_presets(window, cx);
        } else {
            self.show_goto = false;
            self.dismiss_path_editing(cx);
            self.show_presets = true;
            self.set_presets_search_silently("", cx);
            self.refresh_filtered_presets(true, cx);
            self.presets_search
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            cx.notify();
        }
    }

    fn close_presets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_presets(cx);
        self.focus_visible_pane(self.active_pane, window, cx);
    }

    fn apply_selected_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.show_presets {
            return;
        }
        if let Some(matched) = self.filtered_presets.get(self.preset_index) {
            self.apply_preset(matched.index, window, cx);
        }
    }

    fn apply_preset(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preset) = self.config.search.presets.get(index).cloned() else {
            return;
        };
        self.input
            .update(cx, |input, cx| input.emplace(&preset.pattern, cx));
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.options.mode = preset.mode;
            tab.options.highlight_matches = preset.highlight_match;
        }
        self.dismiss_presets(cx);
        self.preset_index = index;
        self.start_search(self.active, cx);
        self.focus_visible_pane(self.active_pane, window, cx);
    }

    fn navigate_presets(&mut self, command: Command, cx: &mut Context<Self>) {
        let len = self.filtered_presets.len();
        if len == 0 {
            self.preset_index = 0;
            return;
        }
        let current = self.preset_index.min(len - 1);
        self.preset_index = match command {
            Command::NextLine => (current + 1) % len,
            Command::PreviousLine => (current + len - 1) % len,
            Command::FirstLine => 0,
            Command::LastLine => len - 1,
            _ => current,
        };
        self.presets_scroll.scroll_to_item(self.preset_index);
        cx.notify();
    }

    fn cmd_icon(
        &self,
        id: &'static str,
        icon: Icon,
        tooltip: impl Into<SharedString>,
        command: Command,
        tone: ButtonTone,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        icon_button(
            id,
            icon,
            tooltip,
            self.theme.shortcut(command),
            tone,
            &self.theme,
        )
        .on_click(cx.listener(move |this, _, window, cx| this.command(command, window, cx)))
    }

    fn scrollbar_state(&self, id: ScrollbarId) -> &ScrollbarTrackState {
        match (id.pane, id.axis) {
            (Pane::Main, ScrollAxis::Vertical) => &self.main_vscroll_state,
            (Pane::Main, ScrollAxis::Horizontal) => &self.main_hscroll_state,
            (Pane::Results, ScrollAxis::Vertical) => &self.results_vscroll_state,
            (Pane::Results, ScrollAxis::Horizontal) => &self.results_hscroll_state,
        }
    }

    fn clear_scrollbar_states(&self) {
        self.main_vscroll_state.clear();
        self.main_hscroll_state.clear();
        self.results_vscroll_state.clear();
        self.results_hscroll_state.clear();
    }

    fn row_layouts(&self, pane: Pane) -> &RowLayoutCache {
        match pane {
            Pane::Main => &self.main_row_layouts,
            Pane::Results => &self.results_row_layouts,
        }
    }

    fn clear_row_layouts(&self) {
        self.main_row_layouts.clear();
        self.results_row_layouts.clear();
    }

    fn overview_lane_width(&self) -> f32 {
        (self.config.viewer.overview_width * 0.5).clamp(6.0, 24.0)
    }

    fn main_rail_width(&self) -> f32 {
        if self.config.viewer.show_overview {
            self.overview_lane_width() + DEFAULT_SCROLLBAR_WIDTH
        } else {
            DEFAULT_SCROLLBAR_WIDTH
        }
    }

    fn results_height_bounds(&self, window: &Window) -> (f32, f32) {
        let viewport_h = f32::from(window.viewport_size().height);
        let has_error = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.error.is_some());
        results_height_bounds(viewport_h, self.config.window.show_status_bar, has_error)
    }

    fn effective_results_height(&self, window: &Window) -> f32 {
        let (min_h, max_h) = self.results_height_bounds(window);
        self.results_height.clamp(min_h, max_h)
    }

    fn sync_focus_for_visibility(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let (min_h, max_h) = self.results_height_bounds(window);
        let eff = self.results_height.clamp(min_h, max_h);
        let main_hidden = is_main_hidden(eff, max_h);
        let results_hidden = is_results_hidden(eff);
        if main_hidden && !results_hidden {
            if self.active_pane == Pane::Main {
                self.active_pane = Pane::Results;
            }
            if self.focus.is_focused(window) {
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.result_selection.is_empty()
                    && !tab.results.visible.is_empty()
                {
                    let row = tab
                        .results
                        .visible
                        .binary_search(&tab.selected)
                        .unwrap_or_else(|idx| idx.min(tab.results.visible.len() - 1));
                    tab.result_selection.select_single(row);
                    tab.result_scroll
                        .scroll_to_item(row, ScrollStrategy::Nearest);
                }
                self.results_focus.focus(window, cx);
            }
        } else if results_hidden && !main_hidden {
            if self.active_pane == Pane::Results {
                self.active_pane = Pane::Main;
            }
            if self.results_focus.is_focused(window) {
                self.focus.focus(window, cx);
            }
        }
    }

    fn schedule_resize_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.resize_frame_scheduled {
            return;
        }
        self.resize_frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.resize_frame_scheduled = false;
            this.tick_resize_animation(window, cx);
        });
    }

    fn tick_resize_animation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() || self.results_resize_drag.is_some() {
            self.resize_motion.cancel();
            return;
        }
        let bounds = self.results_height_bounds(window);
        let now = cx.background_executor().now();
        let reduce_motion = cx.reduce_motion();
        let step = self.resize_motion.advance_frame(
            &mut self.results_height,
            &mut self.target_results_height,
            bounds,
            now,
            reduce_motion,
        );
        self.sync_focus_for_visibility(window, cx);
        if step.keep_animating {
            self.schedule_resize_frame(window, cx);
        }
        if step.changed || !step.keep_animating {
            cx.notify();
        }
    }

    fn resize_filter_height_keyboard(
        &mut self,
        direction: ResizeDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.is_empty() {
            self.resize_motion.cancel();
            return;
        }
        let bounds = self.results_height_bounds(window);
        let now = cx.background_executor().now();
        let schedule = self.resize_motion.on_key_trigger(
            direction,
            self.results_height,
            &mut self.target_results_height,
            bounds,
            now,
        );
        if cx.reduce_motion() {
            self.results_height = self.target_results_height;
            self.sync_focus_for_visibility(window, cx);
        }
        if schedule {
            self.schedule_resize_frame(window, cx);
        }
        cx.notify();
    }

    fn step_filter_height_click(
        &mut self,
        direction: ResizeDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.is_empty() {
            return;
        }
        let bounds = self.results_height_bounds(window);
        let now = cx.background_executor().now();
        self.resize_motion.step_once(
            direction,
            self.results_height,
            &mut self.target_results_height,
            bounds,
            now,
        );
        if cx.reduce_motion() {
            self.resize_motion.cancel();
            self.results_height = self.target_results_height;
            self.sync_focus_for_visibility(window, cx);
        } else {
            self.schedule_resize_frame(window, cx);
        }
        cx.notify();
    }

    fn reset_filter_height(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.results_resize_drag = None;
        let bounds = self.results_height_bounds(window);
        let now = cx.background_executor().now();
        self.resize_motion.animate_to(
            self.config.window.results_height,
            &mut self.target_results_height,
            bounds,
            now,
        );
        if cx.reduce_motion() {
            self.resize_motion.cancel();
            self.results_height = self.target_results_height;
            self.sync_focus_for_visibility(window, cx);
        } else {
            self.schedule_resize_frame(window, cx);
        }
        cx.notify();
    }

    fn handle_results_resize_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.is_empty() {
            return;
        }
        self.drag_selecting = None;
        self.text_drag = None;
        self.scrollbar_drag = None;
        self.dismiss_presets(cx);
        if event.click_count >= 2 {
            self.reset_filter_height(window, cx);
            return;
        }
        self.resize_motion.cancel();
        let start_height = self.effective_results_height(window);
        self.results_height = start_height;
        self.target_results_height = start_height;
        self.results_resize_drag = Some(ResultsResizeDrag {
            start_y: f32::from(event.position.y),
            start_height,
        });
        cx.notify();
    }

    fn handle_results_resize_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.results_resize_drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.results_resize_drag = None;
            cx.notify();
            return;
        }
        let bounds = self.results_height_bounds(window);
        let next = height_for_drag(drag, f32::from(event.position.y), bounds);
        if (next - self.results_height).abs() >= 0.5
            || (next - self.target_results_height).abs() >= 0.5
        {
            self.results_height = next;
            self.target_results_height = next;
            self.sync_focus_for_visibility(window, cx);
            cx.notify();
        }
    }

    fn render_results_resize_sash(
        &self,
        main_hidden: bool,
        results_hidden: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let t = &self.theme;
        let dragging = self.results_resize_drag.is_some();
        let active = dragging || self.resize_motion.is_animating();
        let tip_theme = t.clone();
        let tooltip_text = if main_hidden {
            "Drag down or double-click to restore main log"
        } else if results_hidden {
            "Drag up or double-click to restore filter pane"
        } else {
            "Drag to resize filter pane · Double-click to reset"
        };
        let border_color = if active { t.accent } else { t.border };
        let pill_color = if active {
            t.accent
        } else {
            t.border.opacity(0.9)
        };
        let hover_border = t.accent;
        let hover_pill = if active { t.accent } else { t.muted };

        div()
            .id("results-resize-sash")
            .group("results-resize-sash")
            .debug_selector(|| "results-resize-sash".into())
            .h(px(RESIZE_SASH_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.panel)
            .border_t_1()
            .border_color(border_color)
            .cursor_row_resize()
            .hover(move |s| s.border_color(hover_border))
            .child(
                div()
                    .w(px(36.))
                    .h(px(3.))
                    .rounded_full()
                    .bg(pill_color)
                    .group_hover("results-resize-sash", move |s| s.bg(hover_pill)),
            )
            .when(!dragging, |sash| {
                sash.tooltip_show_delay(Duration::from_millis(450))
                    .tooltip(move |_window, cx| {
                        Tooltip::view(tooltip_text, None, tip_theme.clone(), cx)
                    })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.handle_results_resize_down(event, window, cx);
                    cx.stop_propagation();
                }),
            )
    }

    fn render_results_resize_button(
        &self,
        direction: ResizeDirection,
        disabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let (id, icon, tooltip, command) = match direction {
            ResizeDirection::Grow => (
                "results-grow",
                Icon::PanelExpand,
                "Make filter pane taller",
                Command::IncreaseFilterHeight,
            ),
            ResizeDirection::Shrink => (
                "results-shrink",
                Icon::PanelShrink,
                "Make filter pane shorter",
                Command::DecreaseFilterHeight,
            ),
        };
        let t = &self.theme;
        let tip_theme = t.clone();
        let shortcut = t.shortcut(command);
        let fg = if disabled {
            t.muted.opacity(0.35)
        } else {
            t.muted
        };
        let hover_bg = t.hover;
        let hover_fg = t.foreground;

        let mut btn = div()
            .id(id)
            .group(id)
            .debug_selector(move || id.into())
            .size(px(24.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .child(icon.element(ICON_XS, fg).when(!disabled, |el| {
                el.group_hover(id, move |s| s.text_color(hover_fg))
            }))
            .tooltip_show_delay(TOOLTIP_DELAY)
            .tooltip(move |_window, cx| {
                Tooltip::view(tooltip, shortcut.clone(), tip_theme.clone(), cx)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());

        if !disabled {
            btn = btn
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.step_filter_height_click(direction, window, cx);
                }));
        }
        btn
    }

    fn handle_scrollbar_press(
        &mut self,
        id: ScrollbarId,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(layout) = self.scrollbar_state(id).layout() else {
            return;
        };
        self.active_pane = id.pane;
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.dismiss_path_editing(cx);
        match id.pane {
            Pane::Main => self.focus.focus(window, cx),
            Pane::Results => self.results_focus.focus(window, cx),
        }
        let press = resolve_track_press(layout, id.axis, position);
        self.drag_selecting = None;
        self.text_drag = None;
        self.scrollbar_drag = Some(ScrollbarDrag {
            id,
            grab_offset: press.grab_offset,
        });
        if let Some(offset) = press.jump_to
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            let handle = match id.pane {
                Pane::Main => &tab.scroll,
                Pane::Results => &tab.result_scroll,
            };
            set_uniform_list_axis_offset(handle, id.axis, offset);
            if id.pane == Pane::Main && id.axis == ScrollAxis::Vertical {
                tab.follow = false;
            }
        }
        cx.notify();
    }

    fn handle_scrollbar_drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.scrollbar_drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.scrollbar_drag = None;
            cx.notify();
            return;
        }
        let Some(layout) = self.scrollbar_state(drag.id).layout() else {
            return;
        };
        let offset = scroll_offset_for_drag(layout, drag.id.axis, event.position, drag.grab_offset);
        if let Some(tab) = self.tabs.get_mut(self.active) {
            let handle = match drag.id.pane {
                Pane::Main => &tab.scroll,
                Pane::Results => &tab.result_scroll,
            };
            set_uniform_list_axis_offset(handle, drag.id.axis, offset);
            if drag.id.pane == Pane::Main && drag.id.axis == ScrollAxis::Vertical {
                tab.follow = false;
            }
        }
        cx.notify();
    }

    fn render_overview_lane(&self, cx: &Context<Self>) -> Div {
        let t = &self.theme;
        let mut lane = div()
            .debug_selector(|| "main-overview".into())
            .relative()
            .h_full()
            .w(px(self.overview_lane_width()))
            .flex_shrink_0()
            .bg(t.panel)
            .border_l_1()
            .border_color(t.border);

        if let Some(tab) = self.tabs.get(self.active)
            && let Some(log) = &tab.log
        {
            let mut markers = std::collections::BTreeMap::new();
            for &line in &tab.results.matches {
                markers.insert(
                    line.saturating_mul(256) / log.len().max(1),
                    (line, t.accent),
                );
            }
            for &line in &tab.bookmarks {
                markers.insert(
                    line.saturating_mul(256) / log.len().max(1),
                    (line, t.warning),
                );
            }
            for (bucket, (line, color)) in markers {
                let tick_color = color.opacity(0.65);
                let group_name = SharedString::from(format!("overview-tick-{bucket}"));
                lane = lane.child(
                    div()
                        .id(("overview", bucket))
                        .group(group_name.clone())
                        .debug_selector(move || format!("overview-marker-{line}"))
                        .absolute()
                        .top(relative(bucket as f32 / 256.))
                        .left_0()
                        .right_0()
                        .h(px(6.))
                        .mt(px(-2.))
                        .px(px(1.5))
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(tab) = this.tabs.get_mut(this.active) {
                                tab.selected = line;
                                tab.main_selection.select_single(line);
                                tab.main_text_selection = None;
                                tab.follow = false;
                                tab.scroll.scroll_to_item(line, ScrollStrategy::Center);
                            }
                            this.active_pane = Pane::Main;
                            this.show_goto = false;
                            this.dismiss_presets(cx);
                            this.dismiss_path_editing(cx);
                            this.focus.focus(window, cx);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .w_full()
                                .h(px(2.))
                                .rounded_full()
                                .bg(tick_color)
                                .group_hover(group_name, move |s| s.bg(color)),
                        ),
                );
            }
        }
        lane
    }

    fn render_vertical_scrollbar(
        &self,
        pane: Pane,
        scroll: UniformListScrollHandle,
        with_left_border: bool,
        width: f32,
        debug_name: &'static str,
        cx: &Context<Self>,
    ) -> Div {
        let t = &self.theme;
        let id = ScrollbarId {
            pane,
            axis: ScrollAxis::Vertical,
        };
        let track_state = self.scrollbar_state(id).clone();
        let dragging = self.scrollbar_drag.is_some_and(|drag| drag.id == id);
        let view = cx.entity().downgrade();

        div()
            .debug_selector(move || debug_name.into())
            .relative()
            .h_full()
            .w(px(width))
            .flex_shrink_0()
            .bg(t.panel)
            .when(with_left_border, |rail| {
                rail.border_l_1().border_color(t.border)
            })
            .child(scrollbar_canvas(
                scroll,
                ScrollAxis::Vertical,
                track_state,
                dragging,
                t.clone(),
                move |pos, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.handle_scrollbar_press(id, pos, window, cx);
                    });
                },
            ))
    }

    fn render_horizontal_scrollbar(
        &self,
        pane: Pane,
        scroll: UniformListScrollHandle,
        debug_name: &'static str,
        cx: &Context<Self>,
    ) -> Div {
        let id = ScrollbarId {
            pane,
            axis: ScrollAxis::Horizontal,
        };
        let track_state = self.scrollbar_state(id).clone();
        let dragging = self.scrollbar_drag.is_some_and(|drag| drag.id == id);
        let view = cx.entity().downgrade();

        div()
            .debug_selector(move || debug_name.into())
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(HORIZONTAL_BAR_HEIGHT))
            .child(scrollbar_canvas(
                scroll,
                ScrollAxis::Horizontal,
                track_state,
                dragging,
                self.theme.clone(),
                move |pos, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.handle_scrollbar_press(id, pos, window, cx);
                    });
                },
            ))
    }

    fn rows(
        &mut self,
        range: std::ops::Range<usize>,
        results: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Vec::new();
        };
        let Some(log) = tab.log.clone() else {
            return Vec::new();
        };
        let pane = if results { Pane::Results } else { Pane::Main };
        let pane_focused = self.active_pane_for(window) == pane;
        let t = self.theme.clone();
        let highlight_regex = if tab.options.highlight_matches && !tab.options.invert {
            tab.highlight_regex.as_ref()
        } else {
            None
        };
        let search_mode = tab.options.mode;
        let max_display_chars = self.config.viewer.max_display_chars;
        let tab_width = self.config.viewer.tab_width;
        let metrics = self.zoom_metrics();
        let row_height = px(metrics.line_height);
        let frame = self.render_frame;
        let cache = self.row_layouts(pane).clone();
        let text_selection = tab.text_selection(pane);
        let text_selection_color = if pane_focused {
            t.text_selection
        } else {
            t.text_selection_soft
        };
        let view = cx.entity().downgrade();
        range
            .filter_map(|row| {
                let line = if results {
                    *tab.results.visible.get(row)?
                } else {
                    row
                };
                let (text, selectable_len, highlight_ranges) = format_display_line(
                    &log,
                    line,
                    max_display_chars,
                    tab_width,
                    highlight_regex,
                    search_mode,
                );
                let text_color: Hsla = self
                    .highlights
                    .iter()
                    .find(|(pattern, _)| pattern.is_match(&text))
                    .map(|(_, color)| rgb_to_hsla(rgb(*color)))
                    .unwrap_or(t.foreground);
                let highlights = if highlight_ranges.is_empty() {
                    Vec::new()
                } else {
                    let match_style = HighlightStyle {
                        background_color: Some(t.match_highlight_bg),
                        color: Some(t.match_highlight_fg),
                        ..Default::default()
                    };
                    highlight_ranges
                        .into_iter()
                        .map(|range| (range, match_style))
                        .collect()
                };
                let row_text_selection = text_selection
                    .and_then(|sel| sel.byte_range_for_row(row, &text[..selectable_len]));
                let view_for_press = view.clone();
                let line_content = SelectableRowText::new(
                    text,
                    selectable_len,
                    highlights,
                    row,
                    row_height,
                    frame,
                    cache.clone(),
                    row_text_selection,
                    text_selection_color,
                    move |press, window, cx| {
                        view_for_press
                            .update(cx, |this, cx| {
                                this.handle_text_mouse_down(pane, press, window, cx)
                            })
                            .unwrap_or(false)
                    },
                );
                let selection = if results {
                    &tab.result_selection
                } else {
                    &tab.main_selection
                };
                let selected = selection.contains(row);
                let is_cursor = !selection.is_empty() && selection.cursor == row;
                let marked = tab.bookmarks.contains(&line);
                let matched = tab.results.matches.binary_search(&line).is_ok();
                let row_bg = if selected {
                    if pane_focused {
                        t.selection
                    } else {
                        t.selection_soft
                    }
                } else {
                    t.background
                };
                let hover_bg = if selected { row_bg } else { t.selection_soft };
                let border_color = if is_cursor {
                    if pane_focused {
                        t.accent
                    } else {
                        t.accent.opacity(0.55)
                    }
                } else if selected {
                    if pane_focused {
                        t.accent.opacity(0.45)
                    } else {
                        t.accent.opacity(0.25)
                    }
                } else {
                    gpui::transparent_black()
                };

                Some(
                    div()
                        .id((if results { "result" } else { "line" }, line))
                        .debug_selector(move || {
                            format!("{}-row-{row}", if results { "result" } else { "main" })
                        })
                        .flex()
                        .items_center()
                        .h(row_height)
                        .min_w_full()
                        .whitespace_nowrap()
                        .font(t.mono_font_spec())
                        .text_size(px(metrics.font_size))
                        .bg(row_bg)
                        .hover(move |s| s.bg(hover_bg))
                        .border_l_2()
                        .border_color(border_color)
                        .text_color(text_color)
                        .cursor_pointer()
                        .child(
                            div()
                                .id((if results { "rmark" } else { "mark" }, line))
                                .w(px(metrics.mark_gutter_width))
                                .h_full()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(marked, |g| {
                                    g.child(
                                        Icon::BookmarkFilled
                                            .element(px(metrics.bookmark_icon_size), t.warning),
                                    )
                                })
                                .when(!marked && matched, |g| {
                                    g.child(
                                        Icon::MatchDot
                                            .element(px(metrics.match_dot_size), t.accent),
                                    )
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _window, cx| {
                                        this.toggle_bookmark_line(line, cx);
                                        cx.stop_propagation();
                                    }),
                                ),
                        )
                        .when(self.config.viewer.line_numbers, |row| {
                            row.child(
                                div()
                                    .w(px(metrics.line_number_width))
                                    .h_full()
                                    .pr(px(metrics.line_number_padding))
                                    .mr(px(metrics.line_number_padding))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .border_r_1()
                                    .border_color(t.border.opacity(0.6))
                                    .text_size(px(metrics.line_number_size))
                                    .text_color(if selected { t.foreground } else { t.muted })
                                    .child(format!("{}", line + 1)),
                            )
                        })
                        .child(
                            div().pr_4().child(
                                div()
                                    .debug_selector(move || {
                                        format!(
                                            "{}-text-{row}",
                                            if results { "result" } else { "main" }
                                        )
                                    })
                                    .child(line_content),
                            ),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                this.handle_row_mouse_down(pane, row, event.modifiers, window, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_move(cx.listener(
                            move |this, event: &MouseMoveEvent, _window, cx| {
                                this.handle_row_mouse_move(pane, row, event, cx);
                            },
                        ))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn toggle_window_maximize(&mut self, window: &mut Window) {
        if window.is_fullscreen() {
            return;
        }
        #[cfg(not(test))]
        window.zoom_window();
        #[cfg(test)]
        {
            self.header_zoom_count += 1;
        }
    }

    fn handle_header_click(&mut self, event: &ClickEvent, window: &mut Window) {
        if !event.standard_click() || event.is_keyboard() {
            self.header_click_armed = false;
            return;
        }
        match event.click_count() {
            1 => {
                self.header_click_armed = true;
            }
            2 if self.header_click_armed => {
                self.header_click_armed = false;
                self.toggle_window_maximize(window);
            }
            _ => {
                self.header_click_armed = false;
            }
        }
    }

    fn render_zoom_button(
        &self,
        id: &'static str,
        icon: Icon,
        tooltip: &'static str,
        command: Command,
        disabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let t = &self.theme;
        let tip_theme = t.clone();
        let shortcut = t.shortcut(command);
        let fg = if disabled {
            t.muted.opacity(0.35)
        } else {
            t.muted
        };
        let hover_bg = t.hover;
        let hover_fg = t.foreground;

        let mut btn = div()
            .id(id)
            .group(id)
            .debug_selector(move || id.into())
            .size(px(32.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(gpui::transparent_black())
            .child(icon.element(ICON_SM, fg).when(!disabled, |el| {
                el.group_hover(id, move |s| s.text_color(hover_fg))
            }))
            .tooltip_show_delay(TOOLTIP_DELAY)
            .tooltip(move |_window, cx| {
                Tooltip::view(tooltip, shortcut.clone(), tip_theme.clone(), cx)
            });

        if !disabled {
            btn = btn
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.command(command, window, cx);
                }));
        }
        btn
    }

    fn render_header(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let t = &self.theme;
        let pane = self.active_pane_for(window);
        let active_tab = self.tabs.get(self.active);
        let has_tabs = !self.tabs.is_empty();
        let follow = active_tab.is_some_and(|tab| tab.follow);
        let selected_count = active_tab.map_or(0, |tab| Self::selection_count(tab, pane));
        let has_text_selection = active_tab.is_some_and(|tab| tab.has_text_selection(pane));
        let is_bookmarked = active_tab.is_some_and(|tab| Self::selection_all_bookmarked(tab, pane));

        let mut tab_strip = div()
            .id("tabs")
            .debug_selector(|| "tab-strip".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .gap_1()
            .overflow_x_scroll();

        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let filename = file_label(&tab.path);
            let full_path = SharedString::from(tab.path.display().to_string());
            let tip_theme = t.clone();
            let close_theme = t.clone();
            let close_shortcut = if active {
                t.shortcut(Command::CloseTab)
            } else {
                None
            };

            tab_strip = tab_strip.child(
                div()
                    .id(("tab", tab.id))
                    .debug_selector(move || format!("tab-{index}"))
                    .h(px(30.))
                    .pl_2p5()
                    .pr_1p5()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if active {
                        t.border
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(if active {
                        t.background
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if active { t.foreground } else { t.muted })
                    .cursor_pointer()
                    .hover({
                        let hover = t.hover;
                        let fg = t.foreground;
                        move |s| {
                            if active {
                                s
                            } else {
                                s.bg(hover).text_color(fg)
                            }
                        }
                    })
                    .child(Icon::FileText.element(ICON_XS, if active { t.accent } else { t.muted }))
                    .child(
                        div()
                            .text_size(t.small_size)
                            .font_weight(if active {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .whitespace_nowrap()
                            .child(filename),
                    )
                    .child(
                        div()
                            .id(("tab-close", tab.id))
                            .debug_selector(move || format!("tab-close-{index}"))
                            .size(px(18.))
                            .rounded_sm()
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover({
                                let hover = t.hover;
                                move |s| s.bg(hover)
                            })
                            .child(Icon::Close.element(px(11.), t.muted))
                            .tooltip_show_delay(TOOLTIP_DELAY)
                            .tooltip(move |_window, cx| {
                                Tooltip::view(
                                    "Close tab",
                                    close_shortcut.clone(),
                                    close_theme.clone(),
                                    cx,
                                )
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.header_click_armed = false;
                                    cx.stop_propagation();
                                }),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_tab_at(index, cx);
                            })),
                    )
                    .tooltip_show_delay(Duration::from_millis(450))
                    .tooltip(move |_window, cx| {
                        Tooltip::view(full_path.clone(), None, tip_theme.clone(), cx)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.header_click_armed = false;
                            cx.stop_propagation();
                        }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.switch(index, cx)))
                    .on_aux_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if event.is_middle_click() {
                            cx.stop_propagation();
                            this.close_tab_at(index, cx);
                        }
                    })),
            );
        }

        let header_left_padding = if cfg!(target_os = "macos") && !window.is_fullscreen() {
            px(80.)
        } else {
            px(12.)
        };

        div()
            .id("header")
            .debug_selector(|| "header".into())
            .h(px(46.))
            .pl(header_left_padding)
            .pr_3()
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| {
                this.header_click_armed = false;
            }))
            .on_click(cx.listener(|this, event: &ClickEvent, window, _cx| {
                this.handle_header_click(event, window);
            }))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_3()
            .bg(t.panel)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .debug_selector(|| "header-brand".into())
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(Icon::NoxMark.element(ICON_MD, t.accent))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.foreground)
                            .child("Nox"),
                    ),
            )
            .when(has_tabs, |header| header.child(toolbar_divider(t)))
            .child(tab_strip)
            .child(
                div()
                    .debug_selector(|| "header-actions".into())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.header_click_armed = false;
                            cx.stop_propagation();
                        }),
                    )
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .child(self.cmd_icon(
                        "open",
                        Icon::FolderOpen,
                        "Open files",
                        Command::Open,
                        if has_tabs {
                            ButtonTone::Ghost
                        } else {
                            ButtonTone::Primary
                        },
                        cx,
                    ))
                    .child(
                        self.cmd_icon(
                            "library",
                            Icon::ClockHistory,
                            "Recent & favorites",
                            Command::ShowFiles,
                            if self.show_files {
                                ButtonTone::Active
                            } else {
                                ButtonTone::Ghost
                            },
                            cx,
                        )
                        .debug_selector(|| "header-library".into()),
                    )
                    .when(has_tabs, |actions| {
                        actions
                            .child(toolbar_divider(t))
                            .child(self.cmd_icon(
                                "reload",
                                Icon::Refresh,
                                "Reload file",
                                Command::Reload,
                                ButtonTone::Ghost,
                                cx,
                            ))
                            .child(
                                self.cmd_icon(
                                    "follow",
                                    Icon::FollowTail,
                                    if follow {
                                        "Following new lines"
                                    } else {
                                        "Follow new lines"
                                    },
                                    Command::ToggleFollow,
                                    if follow {
                                        ButtonTone::Active
                                    } else {
                                        ButtonTone::Ghost
                                    },
                                    cx,
                                )
                                .debug_selector(|| "header-follow".into()),
                            )
                            .child(self.cmd_icon(
                                "mark",
                                if is_bookmarked {
                                    Icon::BookmarkFilled
                                } else {
                                    Icon::Bookmark
                                },
                                if is_bookmarked {
                                    if selected_count > 1 {
                                        format!("Remove {} bookmarks", format_count(selected_count))
                                    } else {
                                        "Remove bookmark".to_owned()
                                    }
                                } else if selected_count > 1 {
                                    format!("Bookmark {} lines", format_count(selected_count))
                                } else {
                                    "Bookmark line".to_owned()
                                },
                                Command::ToggleBookmark,
                                if is_bookmarked {
                                    ButtonTone::Active
                                } else {
                                    ButtonTone::Ghost
                                },
                                cx,
                            ))
                            .child(
                                icon_button(
                                    "goto",
                                    Icon::Hash,
                                    "Go to line",
                                    t.shortcut(Command::GoToLine),
                                    if self.show_goto {
                                        ButtonTone::Active
                                    } else {
                                        ButtonTone::Ghost
                                    },
                                    t,
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.toggle_goto(window, cx)),
                                ),
                            )
                            .child(self.cmd_icon(
                                "copy",
                                Icon::Copy,
                                if has_text_selection {
                                    "Copy selected text".to_owned()
                                } else if selected_count > 1 {
                                    format!("Copy {} selected lines", format_count(selected_count))
                                } else {
                                    "Copy selected line".to_owned()
                                },
                                if has_text_selection {
                                    Command::CopySelectedText
                                } else {
                                    Command::CopyLine
                                },
                                ButtonTone::Ghost,
                                cx,
                            ))
                            .child(toolbar_divider(t))
                            .child(self.render_zoom_button(
                                "zoom-out",
                                Icon::ZoomOut,
                                "Zoom out",
                                Command::ZoomOut,
                                self.zoom_percent <= MIN_ZOOM_PERCENT,
                                cx,
                            ))
                            .child(self.render_zoom_button(
                                "zoom-in",
                                Icon::ZoomIn,
                                "Zoom in",
                                Command::ZoomIn,
                                self.zoom_percent >= MAX_ZOOM_PERCENT,
                                cx,
                            ))
                    }),
            )
    }

    fn render_file_drawer(&self, cx: &Context<Self>) -> Stateful<Div> {
        let t = &self.theme;
        let favorites: Vec<PathBuf> = self
            .config
            .files
            .favorites
            .iter()
            .map(PathBuf::from)
            .collect();
        let has_entries = !favorites.is_empty() || !self.recent.is_empty();

        let mut list = div()
            .id("file-library")
            .flex()
            .flex_col()
            .max_h(px(220.))
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .gap_1()
            .bg(t.elevated)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(t.small_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.muted)
                            .child(Icon::ClockHistory.element(ICON_XS, t.muted))
                            .child("Recent & favorites"),
                    )
                    .child(
                        icon_button(
                            "close-library",
                            Icon::Close,
                            "Close",
                            None,
                            ButtonTone::Ghost,
                            t,
                        )
                        .size(px(24.))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_files = false;
                            cx.notify();
                        })),
                    ),
            );

        if !has_entries {
            list = list.child(
                div()
                    .py_3()
                    .px_2()
                    .text_size(t.small_size)
                    .text_color(t.muted)
                    .child("No recent files yet"),
            );
        }

        for (index, (is_fav, path)) in favorites
            .into_iter()
            .map(|p| (true, p))
            .chain(self.recent.iter().cloned().map(|p| (false, p)))
            .enumerate()
        {
            let name = file_label(&path);
            let dir = parent_label(&path);
            let hover = t.hover;
            list = list.child(
                div()
                    .id(("library-file", index))
                    .h(px(34.))
                    .px_2p5()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(
                        if is_fav {
                            Icon::StarFilled
                        } else {
                            Icon::FileText
                        }
                        .element(ICON_XS, if is_fav { t.warning } else { t.muted }),
                    )
                    .child(
                        div()
                            .text_size(t.small_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.foreground)
                            .flex_shrink_0()
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font(t.mono_font_spec())
                            .text_size(t.xs_size)
                            .text_color(t.muted)
                            .child(dir),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.open(path.clone(), cx))),
            );
        }

        list
    }

    fn render_path_completions(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let t = &self.theme;
        let menu_w = (f32::from(window.viewport_size().width) - 48.0).clamp(280.0, 560.0);
        let highlighted = self.path_completion_index;
        let count_label = if self.path_completions.total_matches > self.path_completions.items.len()
            || self.path_completions.scan_truncated
        {
            format!("{}+", format_count(self.path_completions.items.len()))
        } else {
            format_count(self.path_completions.items.len())
        };
        let new_tab_key = t.shortcut(Command::OpenPathInNewTab).unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "⌘↵"
            } else {
                "Ctrl+↵"
            }
            .into()
        });

        let mut list = div()
            .id("path-completions-list")
            .max_h(px(260.))
            .overflow_y_scroll()
            .track_scroll(&self.path_completions_scroll)
            .flex()
            .flex_col()
            .gap_0p5();

        for (index, item) in self.path_completions.items.iter().enumerate() {
            let item_highlighted = highlighted == Some(index);
            let hover = t.hover;
            let is_dir = item.is_dir;

            list = list.child(
                div()
                    .id(("path-completion", index))
                    .debug_selector(move || format!("path-completion-{index}"))
                    .h(px(28.))
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if item_highlighted {
                        t.accent_border
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(if item_highlighted {
                        t.selection
                    } else {
                        gpui::transparent_black()
                    })
                    .cursor_pointer()
                    .hover(move |s| if item_highlighted { s } else { s.bg(hover) })
                    .flex()
                    .items_center()
                    .gap_2()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.path_completion_index != Some(index) {
                            this.path_completion_index = Some(index);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.activate_path_completion(
                            index,
                            event.modifiers().secondary(),
                            window,
                            cx,
                        );
                    }))
                    .child(if is_dir {
                        Icon::Folder.element(ICON_XS, t.accent)
                    } else {
                        Icon::FileText.element(ICON_XS, t.muted)
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font(t.mono_font_spec())
                            .text_size(t.small_size)
                            .font_weight(if is_dir {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if is_dir { t.accent } else { t.foreground })
                            .child(item.label.clone()),
                    ),
            );
        }

        let out_bar_bounds = self.path_bar_bounds.clone();
        div()
            .id("path-completions-menu")
            .debug_selector(|| "path-completions-menu".into())
            .occlude()
            .absolute()
            .left_0()
            .top(px(26.))
            .w(px(menu_w))
            .p_1p5()
            .rounded_lg()
            .bg(t.elevated)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_1()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                if !out_bar_bounds.get().contains(&event.position) {
                    this.close_path_completions();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .px_2()
                    .pt_0p5()
                    .pb_1()
                    .border_b_1()
                    .border_color(t.border.opacity(0.6))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .min_w_0()
                            .flex_1()
                            .child(Icon::Folder.element(px(11.), t.muted))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font(t.mono_font_spec())
                                    .text_size(px(10.5))
                                    .text_color(t.muted)
                                    .child(self.path_completions.directory_label.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(t.background)
                                    .font(t.mono_font_spec())
                                    .text_size(px(10.))
                                    .text_color(t.muted)
                                    .child(count_label),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font(t.mono_font_spec())
                            .text_size(px(10.))
                            .text_color(t.muted)
                            .child(format!("Tab · ↑↓ · ↵ · {new_tab_key}")),
                    ),
            )
            .child(list)
    }

    fn render_file_path_bar(
        &self,
        main_focused: bool,
        main_hidden: bool,
        loading: bool,
        line_count: usize,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let t = &self.theme;
        let path_focused = self.path_input_focused(window, cx);
        let bar_bounds = self.path_bar_bounds.clone();
        let bounds_tracker = canvas(
            move |bounds, _window, _cx| {
                bar_bounds.set(bounds);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let replace_key = t
            .shortcut(Command::ReplaceLogPath)
            .unwrap_or_else(|| "↵".into());
        let new_tab_key = t.shortcut(Command::OpenPathInNewTab).unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "⌘↵"
            } else {
                "Ctrl+↵"
            }
            .into()
        });

        let path_editor = div()
            .id("file-path-wrapper")
            .relative()
            .flex_1()
            .min_w_0()
            .key_context("FilePath")
            .on_action(cx.listener(|this, _: &input_actions::Enter, window, cx| {
                this.submit_file_path(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &input_actions::Tab, window, cx| {
                this.complete_file_path(window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" && event.keystroke.modifiers.secondary() {
                    cx.stop_propagation();
                    this.submit_file_path(true, window, cx);
                } else if event.keystroke.key == "tab" && event.keystroke.modifiers.shift {
                    cx.stop_propagation();
                    this.navigate_path_completions(false, cx);
                }
            }))
            .child(
                div()
                    .relative()
                    .h(px(22.))
                    .px_1p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(if path_focused {
                        t.accent_border
                    } else {
                        gpui::transparent_black()
                    })
                    .bg(if path_focused {
                        t.panel
                    } else {
                        gpui::transparent_black()
                    })
                    .when(!path_focused, |el| {
                        let hover_bg = t.hover.opacity(0.5);
                        let hover_border = t.border.opacity(0.7);
                        el.hover(move |s| s.bg(hover_bg).border_color(hover_border))
                    })
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.path_input.read(cx).focus_handle(cx).focus(window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(bounds_tracker)
                    .child(
                        text_input("file-path-input")
                            .debug_selector(|| "file-path-input".into())
                            .key_context(format!("{DEFAULT_INPUT_CONTEXT} FilePath").as_str())
                            .state(self.path_input.downgrade())
                            .placeholder("Enter file path…")
                            .caret_color(t.accent)
                            .placeholder_color(t.muted)
                            .font(t.mono_font_spec())
                            .text_size(t.xs_size)
                            .text_color(if main_focused || path_focused {
                                t.foreground
                            } else {
                                t.muted
                            })
                            .line_height(relative(1.35))
                            .min_w_0()
                            .w_full()
                            .whitespace_nowrap()
                            .overflow_x_scroll(),
                    ),
            )
            .when(
                path_focused
                    && self.show_path_completions
                    && !self.path_completions.items.is_empty(),
                |el| el.child(deferred(self.render_path_completions(window, cx)).priority(1)),
            );

        div()
            .debug_selector(|| "file-path-bar".into())
            .h(px(FILE_PATH_BAR_HEIGHT))
            .px_3()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .bg(t.background)
            .when(!main_hidden || path_focused, |el| el.border_b_1())
            .border_color(if path_focused {
                t.accent_border
            } else {
                t.border.opacity(0.6)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus_visible_pane(Pane::Main, window, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .min_w_0()
                    .flex_1()
                    .child(div().w(px(2.)).h(px(12.)).rounded_full().bg(
                        if main_focused || path_focused {
                            t.accent
                        } else {
                            gpui::transparent_black()
                        },
                    ))
                    .child(Icon::Lines.element(
                        px(12.),
                        if main_focused || path_focused {
                            t.accent
                        } else {
                            t.muted
                        },
                    ))
                    .child(path_editor),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .font(t.mono_font_spec())
                    .text_size(t.xs_size)
                    .text_color(t.muted)
                    .when(path_focused, |el| {
                        el.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .text_size(px(10.))
                                .text_color(t.muted)
                                .child(
                                    div()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(t.panel)
                                        .border_1()
                                        .border_color(t.border)
                                        .child(replace_key),
                                )
                                .child("Replace")
                                .child(
                                    div()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(t.panel)
                                        .border_1()
                                        .border_color(t.border)
                                        .child(new_tab_key),
                                )
                                .child("New tab"),
                        )
                    })
                    .when(loading, |el| el.text_color(t.accent).child("Indexing…"))
                    .child(format_count(line_count)),
            )
    }

    fn render_goto_popover(&self, line_count: usize, cx: &Context<Self>) -> Stateful<Div> {
        let t = &self.theme;
        let right_inset = if line_count > 0 {
            self.main_rail_width() + 12.
        } else {
            12.
        };

        div()
            .id("goto-popover")
            .debug_selector(|| "goto-popover".into())
            .key_context("GoTo")
            .occlude()
            .absolute()
            .top(px(8.))
            .right(px(right_inset))
            .h(px(40.))
            .px_2p5()
            .rounded_lg()
            .bg(t.elevated)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .flex()
            .items_center()
            .gap_2()
            .child(Icon::Hash.element(ICON_SM, t.accent))
            .child(
                div()
                    .w(px(176.))
                    .h(px(28.))
                    .px_2()
                    .rounded_md()
                    .bg(t.background)
                    .border_1()
                    .border_color(t.accent_border)
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .child(
                        text_input("goto-input")
                            .debug_selector(|| "goto-input".into())
                            .key_context(format!("{DEFAULT_INPUT_CONTEXT} GoTo").as_str())
                            .state(self.goto.downgrade())
                            .placeholder("Line number…")
                            .caret_color(t.accent)
                            .placeholder_color(t.muted)
                            .font(t.mono_font_spec())
                            .text_size(t.mono_size)
                            .line_height(relative(1.35))
                            .min_w_0()
                            .w_full()
                            .whitespace_nowrap()
                            .overflow_x_scroll(),
                    ),
            )
            .child(
                icon_button(
                    "jump",
                    Icon::ArrowRight,
                    "Jump to line",
                    Some("↵".into()),
                    ButtonTone::Primary,
                    t,
                )
                .size(px(28.))
                .on_click(
                    cx.listener(|this, _, window, cx| this.command(Command::GoToLine, window, cx)),
                ),
            )
            .child(
                icon_button(
                    "goto-cancel",
                    Icon::Close,
                    "Cancel",
                    Some("Esc".into()),
                    ButtonTone::Ghost,
                    t,
                )
                .size(px(28.))
                .on_click(
                    cx.listener(|this, _, window, cx| this.command(Command::FocusLog, window, cx)),
                ),
            )
    }

    fn render_error_banner(&self, error: String, cx: &Context<Self>) -> Div {
        let t = &self.theme;
        div()
            .px_3()
            .py_2()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .bg(t.danger_subtle)
            .border_b_1()
            .border_color(t.danger.opacity(0.45))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .flex_1()
                    .child(Icon::AlertTriangle.element(ICON_SM, t.danger))
                    .child(
                        div()
                            .text_size(t.small_size)
                            .text_color(t.foreground)
                            .overflow_hidden()
                            .child(error),
                    ),
            )
            .child(self.cmd_icon(
                "error-reload",
                Icon::Refresh,
                "Retry / reload file",
                Command::Reload,
                ButtonTone::Subtle,
                cx,
            ))
    }

    fn render_welcome(&self, cx: &Context<Self>) -> Div {
        let t = &self.theme;
        let open_shortcut = t.shortcut(Command::Open);
        let quick_files: Vec<(bool, PathBuf)> = self
            .config
            .files
            .favorites
            .iter()
            .map(|p| (true, PathBuf::from(p)))
            .chain(self.recent.iter().cloned().map(|p| (false, p)))
            .take(6)
            .collect();

        let mut hero = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_5()
            .p_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(64.))
                            .rounded_xl()
                            .bg(t.panel)
                            .border_1()
                            .border_color(t.border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::NoxMark.element(ICON_HERO, t.accent)),
                    )
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.foreground)
                            .child("Nox"),
                    )
                    .child(
                        div()
                            .text_size(t.ui_size)
                            .text_color(t.muted)
                            .child("Drop log files anywhere or open to explore"),
                    ),
            )
            .child(
                div()
                    .id("welcome-open")
                    .h(px(40.))
                    .px_4()
                    .rounded_lg()
                    .bg(t.accent)
                    .text_color(t.on_accent)
                    .hover({
                        let accent = t.accent.opacity(0.88);
                        move |s| s.bg(accent)
                    })
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(Icon::FolderOpen.element(ICON_MD, t.on_accent))
                    .child("Open")
                    .when_some(open_shortcut, |el, key| {
                        el.child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .bg(t.on_accent.opacity(0.14))
                                .font(t.mono_font_spec())
                                .text_size(t.xs_size)
                                .child(key),
                        )
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.command(Command::Open, window, cx)),
                    ),
            );

        if !quick_files.is_empty() {
            let mut card = div()
                .w(px(420.))
                .max_w_full()
                .p_2()
                .rounded_lg()
                .bg(t.panel)
                .border_1()
                .border_color(t.border)
                .flex()
                .flex_col()
                .gap_1();

            for (index, (is_fav, path)) in quick_files.into_iter().enumerate() {
                let name = file_label(&path);
                let dir = parent_label(&path);
                let hover = t.hover;
                card = card.child(
                    div()
                        .id(("welcome-file", index))
                        .h(px(34.))
                        .px_2p5()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(
                            if is_fav {
                                Icon::StarFilled
                            } else {
                                Icon::ClockHistory
                            }
                            .element(ICON_XS, if is_fav { t.warning } else { t.muted }),
                        )
                        .child(
                            div()
                                .text_size(t.small_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.foreground)
                                .flex_shrink_0()
                                .child(name),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font(t.mono_font_spec())
                                .text_size(t.xs_size)
                                .text_color(t.muted)
                                .child(dir),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.open(path.clone(), cx))),
                );
            }
            hero = hero.child(card);
        }

        hero
    }

    fn render_presets_dropdown(
        &self,
        options: &SearchConfig,
        current_input: &str,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let t = &self.theme;
        let active_index = self.matching_preset_index(current_input, options);
        let active_name = active_index
            .and_then(|idx| self.config.search.presets.get(idx))
            .map(|preset| preset.name.clone());
        let is_open = self.show_presets;
        let is_active = active_index.is_some();

        let (bg, border, fg, icon_color, hover_bg) = if is_open {
            (
                t.elevated,
                t.accent_border,
                t.foreground,
                t.accent,
                t.elevated,
            )
        } else if is_active {
            (
                t.accent_subtle,
                t.accent_border,
                t.accent,
                t.accent,
                t.accent_subtle,
            )
        } else {
            (t.background, t.border, t.foreground, t.muted, t.hover)
        };

        let trigger_bounds = self.presets_trigger_bounds.clone();
        let bounds_tracker = canvas(
            move |bounds, _window, _cx| {
                trigger_bounds.set(bounds);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();

        let tip_theme = t.clone();
        let shortcut = t.shortcut(Command::TogglePresets);
        let label = active_name.unwrap_or_else(|| "Patterns".to_owned());

        let trigger = div()
            .id("presets-trigger")
            .debug_selector(|| "presets-trigger".into())
            .relative()
            .w(px(180.))
            .h(px(32.))
            .pl_2p5()
            .pr_2()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .flex()
            .items_center()
            .gap_1p5()
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .child(bounds_tracker)
            .child(Icon::ListFilter.element(ICON_XS, icon_color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(t.small_size)
                    .font_weight(FontWeight::MEDIUM)
                    .child(label),
            )
            .child(
                (if is_open {
                    Icon::ChevronUp
                } else {
                    Icon::ChevronDown
                })
                .element(
                    px(12.),
                    if is_open || is_active {
                        t.accent
                    } else {
                        t.muted
                    },
                ),
            )
            .tooltip_show_delay(TOOLTIP_DELAY)
            .tooltip(move |_window, cx| {
                Tooltip::view("Saved patterns", shortcut.clone(), tip_theme.clone(), cx)
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_presets(window, cx)));

        let mut wrapper = div()
            .relative()
            .flex_shrink_0()
            .flex()
            .items_center()
            .child(trigger);

        if is_open {
            let results_h = self.effective_results_height(window);
            let status_h = if self.config.window.show_status_bar {
                26.0
            } else {
                0.0
            };
            let open_upward = results_h + 30.0 + status_h < 220.0;
            let highlighted = self
                .preset_index
                .min(self.filtered_presets.len().saturating_sub(1));
            let filter_query = self.presets_search.read(cx).as_str().to_owned();
            let has_filter = !filter_query.is_empty();
            let is_filtering = !filter_query.trim().is_empty();
            let search_focused = self.presets_search_focused(window, cx);
            let total_count = self.config.search.presets.len();
            let count_badge = if is_filtering {
                format!(
                    "{}/{}",
                    format_count(self.filtered_presets.len()),
                    format_count(total_count)
                )
            } else {
                format_count(total_count)
            };

            let search_field = div()
                .h(px(28.))
                .px_2()
                .rounded_md()
                .bg(t.background)
                .border_1()
                .border_color(if search_focused {
                    t.accent_border
                } else {
                    t.border
                })
                .flex()
                .items_center()
                .gap_1p5()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.presets_search
                            .read(cx)
                            .focus_handle(cx)
                            .focus(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    Icon::Search.element(ICON_XS, if search_focused { t.accent } else { t.muted }),
                )
                .child(
                    div().flex_1().min_w_0().overflow_hidden().child(
                        text_input("presets-search")
                            .debug_selector(|| "presets-search-input".into())
                            .key_context(format!("{DEFAULT_INPUT_CONTEXT} Presets").as_str())
                            .state(self.presets_search.downgrade())
                            .placeholder("Filter patterns…")
                            .caret_color(t.accent)
                            .placeholder_color(t.muted)
                            .font(t.ui_font_spec())
                            .text_size(t.small_size)
                            .line_height(relative(1.35))
                            .min_w_0()
                            .w_full()
                            .whitespace_nowrap()
                            .overflow_x_scroll(),
                    ),
                )
                .when(has_filter, |el| {
                    el.child(
                        icon_button(
                            "clear-presets-search",
                            Icon::Close,
                            "Clear pattern filter",
                            None,
                            ButtonTone::Ghost,
                            t,
                        )
                        .debug_selector(|| "clear-presets-search".into())
                        .size(px(22.))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.clear_presets_search(window, cx)
                            }),
                        ),
                    )
                });

            let mut list = div()
                .id("presets-list")
                .max_h(px(220.))
                .overflow_y_scroll()
                .track_scroll(&self.presets_scroll)
                .flex()
                .flex_col()
                .gap_0p5();

            if self.filtered_presets.is_empty() {
                list = list.child(
                    div()
                        .debug_selector(|| "presets-empty".into())
                        .px_2p5()
                        .py_4()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(t.small_size)
                        .text_color(t.muted)
                        .child("No matching patterns"),
                );
            } else {
                for (row_index, matched) in self.filtered_presets.iter().enumerate() {
                    let preset_index = matched.index;
                    let Some(preset) = self.config.search.presets.get(preset_index) else {
                        continue;
                    };
                    let item_active = active_index == Some(preset_index);
                    let item_highlighted = highlighted == row_index;
                    let hover = t.hover;
                    let mode_label = match preset.mode {
                        SearchMode::Regex => "Regex",
                        SearchMode::Literal => "Literal",
                    };
                    let name_element: AnyElement = if matched.ranges.is_empty() {
                        preset.name.clone().into_any_element()
                    } else {
                        let highlight_style = HighlightStyle {
                            color: Some(t.accent),
                            font_weight: Some(FontWeight::BOLD),
                            background_color: item_active.then_some(t.accent_subtle),
                            ..Default::default()
                        };
                        let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = matched
                            .ranges
                            .iter()
                            .cloned()
                            .map(|range| (range, highlight_style))
                            .collect();
                        StyledText::new(preset.name.clone())
                            .with_highlights(highlights)
                            .into_any_element()
                    };

                    list = list.child(
                        div()
                            .id(("preset-item", preset_index))
                            .debug_selector(move || format!("preset-item-{preset_index}"))
                            .px_2p5()
                            .py_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(if item_highlighted {
                                t.accent_border
                            } else {
                                gpui::transparent_black()
                            })
                            .bg(if item_highlighted {
                                t.selection
                            } else if item_active {
                                t.accent_subtle.opacity(0.5)
                            } else {
                                gpui::transparent_black()
                            })
                            .cursor_pointer()
                            .hover(move |s| if item_highlighted { s } else { s.bg(hover) })
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if this.preset_index != row_index {
                                    this.preset_index = row_index;
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.apply_preset(preset_index, window, cx);
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_size(t.small_size)
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(if item_active {
                                                t.accent
                                            } else {
                                                t.foreground
                                            })
                                            .child(name_element),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded_sm()
                                            .bg(t.background)
                                            .border_1()
                                            .border_color(t.border)
                                            .font(t.mono_font_spec())
                                            .text_size(px(10.))
                                            .text_color(t.muted)
                                            .child(mode_label),
                                    ),
                            )
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .font(t.mono_font_spec())
                                    .text_size(t.xs_size)
                                    .text_color(t.muted)
                                    .child(preset.pattern.clone()),
                            ),
                    );
                }
            }

            let out_trigger_bounds = self.presets_trigger_bounds.clone();
            let mut menu = div()
                .id("presets-menu")
                .debug_selector(|| "presets-menu".into())
                .key_context("Presets")
                .track_focus(&self.presets_focus)
                .on_action(cx.listener(|this, _: &input_actions::Enter, window, cx| {
                    this.apply_selected_preset(window, cx);
                }))
                .occlude()
                .absolute()
                .right_0()
                .w(px(320.))
                .p_1p5()
                .rounded_lg()
                .bg(t.elevated)
                .border_1()
                .border_color(t.border)
                .shadow_lg()
                .flex()
                .flex_col()
                .gap_1()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if !out_trigger_bounds.get().contains(&event.position) {
                        this.dismiss_presets(cx);
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .px_2()
                        .pt_1()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(t.border.opacity(0.6))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .text_size(t.xs_size)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.muted)
                                .child("Saved patterns")
                                .child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded_sm()
                                        .bg(t.background)
                                        .font(t.mono_font_spec())
                                        .text_size(px(10.))
                                        .text_color(t.muted)
                                        .child(count_badge),
                                ),
                        )
                        .child(
                            div()
                                .font(t.mono_font_spec())
                                .text_size(px(10.))
                                .text_color(t.muted)
                                .child("↑↓ ↵"),
                        ),
                )
                .child(search_field)
                .child(list);

            menu = if open_upward {
                menu.bottom(px(36.))
            } else {
                menu.top(px(36.))
            };

            wrapper = wrapper.child(deferred(menu).priority(1));
        }

        wrapper
    }

    fn render_search_bar(
        &self,
        options: &SearchConfig,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let t = &self.theme;
        let current_input = self.input.read(cx).as_str().to_owned();
        let has_query = !current_input.is_empty();
        let regex_on = options.mode == SearchMode::Regex;

        let search_box =
            div()
                .flex_1()
                .min_w(px(200.))
                .h(px(34.))
                .pl_2p5()
                .pr_1()
                .rounded_md()
                .bg(t.background)
                .border_1()
                .border_color(t.border)
                .flex()
                .items_center()
                .gap_1p5()
                .child(Icon::Search.element(ICON_SM, t.muted))
                .child(
                    div()
                        .key_context("Search")
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(
                            text_input("query")
                                .debug_selector(|| "search-input".into())
                                .key_context(format!("{DEFAULT_INPUT_CONTEXT} Search").as_str())
                                .state(self.input.downgrade())
                                .placeholder("Search…")
                                .caret_color(t.accent)
                                .placeholder_color(t.muted)
                                .font(t.mono_font_spec())
                                .text_size(t.mono_size)
                                .line_height(relative(1.35))
                                .min_w_0()
                                .w_full()
                                .whitespace_nowrap()
                                .overflow_x_scroll(),
                        ),
                )
                .when(has_query, |el| {
                    el.child(
                        icon_button(
                            "clear-search",
                            Icon::Close,
                            "Clear search",
                            None,
                            ButtonTone::Ghost,
                            t,
                        )
                        .size(px(24.))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_search(cx))),
                    )
                })
                .child(toolbar_divider(t))
                .child(
                    search_toggle(
                        "regex",
                        Icon::Regex,
                        "Regular expression",
                        t.shortcut(Command::ToggleRegex),
                        regex_on,
                        t,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.command(Command::ToggleRegex, window, cx)
                    })),
                )
                .child(
                    search_toggle(
                        "case",
                        Icon::CaseSensitive,
                        "Match case",
                        t.shortcut(Command::ToggleCaseSensitive),
                        options.case_sensitive,
                        t,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.command(Command::ToggleCaseSensitive, window, cx)
                    })),
                )
                .child(
                    search_toggle(
                        "word",
                        Icon::WholeWord,
                        "Whole words",
                        t.shortcut(Command::ToggleWholeWord),
                        options.whole_word,
                        t,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.command(Command::ToggleWholeWord, window, cx)
                    })),
                )
                .child(
                    search_toggle(
                        "invert",
                        Icon::InvertMatch,
                        "Exclude matches",
                        t.shortcut(Command::ToggleInvert),
                        options.invert,
                        t,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.command(Command::ToggleInvert, window, cx)
                    })),
                );

        let highlight_tooltip = if options.highlight_matches && options.invert {
            "Highlight matches (paused while excluding)"
        } else if options.highlight_matches {
            "Highlighting matches"
        } else {
            "Highlight matches"
        };
        let highlight_tone = if options.highlight_matches && !options.invert {
            ButtonTone::Active
        } else {
            ButtonTone::Ghost
        };

        let mut bar = div()
            .h(px(44.))
            .flex_shrink_0()
            .bg(t.panel)
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .child(
                self.cmd_icon(
                    "highlight-matches",
                    Icon::Highlighter,
                    highlight_tooltip,
                    Command::ToggleHighlightMatches,
                    highlight_tone,
                    cx,
                )
                .debug_selector(|| "highlight-matches".into()),
            )
            .child(search_box)
            .child(self.cmd_icon(
                "prev-match",
                Icon::ChevronUp,
                "Previous match",
                Command::PreviousMatch,
                ButtonTone::Ghost,
                cx,
            ))
            .child(self.cmd_icon(
                "next-match",
                Icon::ChevronDown,
                "Next match",
                Command::NextMatch,
                ButtonTone::Ghost,
                cx,
            ))
            .child(self.cmd_icon(
                "search",
                Icon::ArrowRight,
                "Apply search",
                Command::Search,
                ButtonTone::Primary,
                cx,
            ));

        if !self.config.search.presets.is_empty() {
            bar = bar
                .child(toolbar_divider(t))
                .child(self.render_presets_dropdown(options, &current_input, window, cx));
        }

        bar
    }

    fn render_status_bar(&self, window: &Window, cx: &Context<Self>) -> Div {
        let t = &self.theme;
        let pane = self.active_pane_for(window);
        let tab = self.tabs.get(self.active);
        let line_count = tab.and_then(|tab| tab.log.as_ref()).map_or(0, |l| l.len());
        let selected = tab.map_or(0, |tab| if line_count == 0 { 0 } else { tab.selected + 1 });
        let result_count = tab.map_or(0, |tab| tab.results.visible.len());
        let result_row = tab.map_or(0, |tab| {
            if result_count == 0 {
                0
            } else {
                tab.result_selection.cursor.min(result_count - 1) + 1
            }
        });
        let selected_count = tab.map_or(0, |tab| Self::selection_count(tab, pane));
        let text_selection_label = tab.and_then(|tab| self.selected_text_label(tab, pane));
        let bookmarks = tab.map_or(0, |tab| tab.bookmarks.len());
        let bytes = tab.and_then(|tab| tab.log.as_ref()).map(|l| l.stamp.len);
        let follow = tab.is_some_and(|tab| tab.follow);

        div()
            .h(px(26.))
            .px_3()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.border)
            .text_size(t.xs_size)
            .text_color(t.muted)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .when(tab.is_some(), |left| {
                        left.child(
                            div()
                                .id("status-line")
                                .flex()
                                .items_center()
                                .gap_1()
                                .font(t.mono_font_spec())
                                .cursor_pointer()
                                .hover({
                                    let fg = t.foreground;
                                    move |s| s.text_color(fg)
                                })
                                .child(format!(
                                    "Ln {} / {}",
                                    format_count(selected),
                                    format_count(line_count)
                                ))
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.toggle_goto(window, cx)),
                                ),
                        )
                        .when(pane == Pane::Results && result_count > 0, |el| {
                            el.child(
                                div()
                                    .font(t.mono_font_spec())
                                    .text_color(t.foreground)
                                    .child(format!(
                                        "Filter {} / {}",
                                        format_count(result_row),
                                        format_count(result_count)
                                    )),
                            )
                        })
                        .when_some(text_selection_label, |el, label| {
                            el.child(
                                div()
                                    .font(t.mono_font_spec())
                                    .text_color(t.accent)
                                    .child(label),
                            )
                        })
                        .when(selected_count > 1, |el| {
                            el.child(
                                div()
                                    .font(t.mono_font_spec())
                                    .text_color(t.accent)
                                    .child(format!("{} selected", format_count(selected_count))),
                            )
                        })
                        .child(
                            div()
                                .id("status-bookmarks")
                                .flex()
                                .items_center()
                                .gap_1()
                                .cursor_pointer()
                                .text_color(if bookmarks > 0 { t.warning } else { t.muted })
                                .hover({
                                    let fg = t.foreground;
                                    move |s| s.text_color(fg)
                                })
                                .child(
                                    if bookmarks > 0 {
                                        Icon::BookmarkFilled
                                    } else {
                                        Icon::Bookmark
                                    }
                                    .element(
                                        px(11.),
                                        if bookmarks > 0 { t.warning } else { t.muted },
                                    ),
                                )
                                .child(
                                    div()
                                        .font(t.mono_font_spec())
                                        .child(format_count(bookmarks)),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.command(Command::NextBookmark, window, cx)
                                })),
                        )
                        .when_some(bytes, |el, len| {
                            el.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(div().size(px(6.)).rounded_full().bg(if follow {
                                        t.accent
                                    } else {
                                        t.muted.opacity(0.6)
                                    }))
                                    .child(
                                        div()
                                            .font(t.mono_font_spec())
                                            .child(format!("{} · UTF-8", format_bytes(len))),
                                    ),
                            )
                        })
                    })
                    .when(tab.is_none(), |left| left.child("Ready"))
                    .when(self.zoom_percent != DEFAULT_ZOOM_PERCENT, |left| {
                        let tip_theme = t.clone();
                        let reset_shortcut = t.shortcut(Command::ZoomReset);
                        let fg = t.foreground;
                        left.child(
                            div()
                                .id("status-zoom")
                                .debug_selector(|| "status-zoom".into())
                                .px_1p5()
                                .rounded_sm()
                                .bg(t.elevated)
                                .border_1()
                                .border_color(t.border)
                                .font(t.mono_font_spec())
                                .text_color(t.accent)
                                .cursor_pointer()
                                .hover(move |s| s.text_color(fg))
                                .child(format!("{}%", self.zoom_percent))
                                .tooltip_show_delay(TOOLTIP_DELAY)
                                .tooltip(move |_window, cx| {
                                    Tooltip::view(
                                        "Reset zoom",
                                        reset_shortcut.clone(),
                                        tip_theme.clone(),
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.command(Command::ZoomReset, window, cx)
                                })),
                        )
                    }),
            )
            .when_some(self.status.clone(), |bar, feedback| {
                let (icon, color) = feedback.icon_and_color(t);
                bar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_color(color)
                        .child(icon.element(px(12.), color))
                        .child(feedback.text),
                )
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_frame = self.render_frame.wrapping_add(1);
        self.main_row_layouts.begin_frame(self.render_frame);
        self.results_row_layouts.begin_frame(self.render_frame);

        let t = self.theme.clone();
        let active_pane = self.active_pane_for(window);
        let main_focused = active_pane == Pane::Main;
        let results_focused = active_pane == Pane::Results;
        let tab = self.tabs.get(self.active);
        let widest_line = tab
            .and_then(|tab| tab.log.as_ref())
            .map_or(0, |log| log.widest_line);
        let widest_result = tab.map_or(0, |tab| tab.results.widest_row);
        let line_count = tab
            .and_then(|tab| tab.log.as_ref())
            .map_or(0, |log| log.len());
        let match_count = tab.map_or(0, |tab| tab.results.matches.len());
        let result_count = tab.map_or(0, |tab| tab.results.visible.len());
        let results_limited = tab.is_some_and(|tab| tab.results.limited);
        let loading = tab.is_some_and(|tab| tab.loading);
        let searching = tab.is_some_and(|tab| tab.searching);
        let show_searching = tab.is_some_and(|tab| tab.searching_visible);
        let tab_error = tab.and_then(|tab| tab.error.clone());
        let options = tab
            .map(|tab| tab.options.clone())
            .unwrap_or_else(|| self.config.search.clone());
        let scroll = tab.map(|tab| tab.scroll.clone()).unwrap_or_default();
        let result_scroll = tab.map(|tab| tab.result_scroll.clone()).unwrap_or_default();
        let main_multi_selected = tab.is_some_and(|tab| tab.main_selection.has_multi_selection());
        let results_multi_selected =
            tab.is_some_and(|tab| tab.result_selection.has_multi_selection());
        let main_text_selected = tab.is_some_and(|tab| tab.has_text_selection(Pane::Main));
        let results_text_selected = tab.is_some_and(|tab| tab.has_text_selection(Pane::Results));
        let regex_mode = tab.is_some_and(|tab| tab.options.mode == SearchMode::Regex);
        let (min_results_h, max_results_h) = self.results_height_bounds(window);
        let effective_results_h = self.results_height.clamp(min_results_h, max_results_h);
        let main_hidden = is_main_hidden(effective_results_h, max_results_h);
        let results_hidden = is_results_hidden(effective_results_h);
        if line_count == 0 || main_hidden {
            self.main_vscroll_state.clear();
            self.main_hscroll_state.clear();
            self.main_row_layouts.clear();
        }
        if result_count == 0 || results_hidden {
            self.results_vscroll_state.clear();
            self.results_hscroll_state.clear();
            self.results_row_layouts.clear();
        }

        let main_area = if self.tabs.is_empty() {
            self.render_welcome(cx)
        } else {
            let empty_results = if show_searching || loading {
                "Searching…"
            } else if searching {
                ""
            } else if tab_error.is_some() {
                "Results unavailable"
            } else if tab.is_some_and(|tab| tab.query.is_empty()) {
                if self.config.viewer.include_bookmarks {
                    "Search above or click the gutter to bookmark lines"
                } else {
                    "Enter a query above to filter lines"
                }
            } else {
                "No matches"
            };

            let source_log_area = div().key_context("Log").flex().flex_1().min_h_0().child(
                div()
                    .when(main_multi_selected, |area| area.key_context("Selection"))
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .when(main_text_selected, |area| area.key_context("TextSelected"))
                            .track_focus(&self.focus)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.focus_pane(Pane::Main, window, cx)
                                }),
                            )
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .when(line_count == 0, |area| {
                                area.items_center()
                                    .justify_center()
                                    .text_size(t.small_size)
                                    .text_color(t.muted)
                                    .child(if loading {
                                        "Opening file…"
                                    } else if tab_error.is_some() {
                                        "Cannot read file"
                                    } else {
                                        "Empty file"
                                    })
                            })
                            .when(line_count > 0, |area| {
                                area.child(
                                    div()
                                        .relative()
                                        .flex_1()
                                        .min_w_0()
                                        .h_full()
                                        .child(
                                            uniform_list(
                                                "source",
                                                line_count,
                                                cx.processor(|this, range, window, cx| {
                                                    this.rows(range, false, window, cx)
                                                }),
                                            )
                                            .with_horizontal_sizing_behavior(
                                                ListHorizontalSizingBehavior::Unconstrained,
                                            )
                                            .with_width_from_item(Some(widest_line))
                                            .track_scroll(&scroll)
                                            .size_full(),
                                        )
                                        .child(self.render_horizontal_scrollbar(
                                            Pane::Main,
                                            scroll.clone(),
                                            "main-hscroll",
                                            cx,
                                        )),
                                )
                                .when(self.config.viewer.show_overview, |area| {
                                    area.child(self.render_overview_lane(cx))
                                })
                                .child(
                                    self.render_vertical_scrollbar(
                                        Pane::Main,
                                        scroll.clone(),
                                        !self.config.viewer.show_overview,
                                        DEFAULT_SCROLLBAR_WIDTH,
                                        "main-vscroll",
                                        cx,
                                    ),
                                )
                            }),
                    ),
            );

            let source_pane = div()
                .debug_selector(|| "source-pane".into())
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .child(source_log_area)
                .when(self.show_goto, |body| {
                    body.child(self.render_goto_popover(line_count, cx))
                });

            let at_max_results_h = main_hidden || effective_results_h >= max_results_h - 0.5;
            let at_min_results_h = results_hidden || effective_results_h <= min_results_h + 0.5;

            let results_header =
                div()
                    .h(px(30.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(t.panel)
                    .border_t_1()
                    .when(!results_hidden, |el| el.border_b_1())
                    .border_color(t.border)
                    .when(!results_hidden, |el| {
                        el.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.focus_pane(Pane::Results, window, cx)
                            }),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(t.xs_size)
                            .text_color(t.muted)
                            .child(div().w(px(2.)).h(px(12.)).rounded_full().bg(
                                if results_focused {
                                    t.accent
                                } else {
                                    gpui::transparent_black()
                                },
                            ))
                            .child(Icon::ListFilter.element(
                                ICON_XS,
                                if results_focused || match_count > 0 {
                                    t.accent
                                } else {
                                    t.muted
                                },
                            ))
                            .child(
                                div()
                                    .font(t.mono_font_spec())
                                    .text_color(if result_count > 0 {
                                        t.foreground
                                    } else {
                                        t.muted
                                    })
                                    .child(if result_count == match_count {
                                        format_count(match_count)
                                    } else {
                                        format!(
                                            "{} · {}",
                                            format_count(match_count),
                                            format_count(result_count)
                                        )
                                    }),
                            )
                            .when(results_limited, |el| {
                                let tip_theme = t.clone();
                                el.child(
                                    div()
                                        .id("result-limit")
                                        .flex()
                                        .items_center()
                                        .child(Icon::AlertTriangle.element(ICON_XS, t.warning))
                                        .tooltip_show_delay(TOOLTIP_DELAY)
                                        .tooltip(move |_, cx| {
                                            Tooltip::view(
                                                "Result limit reached",
                                                None,
                                                tip_theme.clone(),
                                                cx,
                                            )
                                        }),
                                )
                            })
                            .when(show_searching, |el| {
                                el.text_color(t.accent).child("Searching…")
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .child(self.render_results_resize_button(
                                ResizeDirection::Grow,
                                at_max_results_h,
                                cx,
                            ))
                            .child(self.render_results_resize_button(
                                ResizeDirection::Shrink,
                                at_min_results_h,
                                cx,
                            ))
                            .when(result_count > 0, |actions| {
                                actions
                                    .child(
                                        div()
                                            .w(px(1.))
                                            .h(px(14.))
                                            .mx_0p5()
                                            .flex_shrink_0()
                                            .bg(t.border),
                                    )
                                    .child(
                                        self.cmd_icon(
                                            "export",
                                            Icon::Download,
                                            "Export results",
                                            Command::ExportResults,
                                            ButtonTone::Ghost,
                                            cx,
                                        )
                                        .size(px(24.)),
                                    )
                            }),
                    );

            let results_body = div()
                .debug_selector(|| "results-body".into())
                .key_context("Log")
                .when(main_hidden, |body| body.flex_1().min_h_0())
                .when(!main_hidden, |body| {
                    body.h(px(effective_results_h)).flex_shrink_0()
                })
                .w_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .when(results_multi_selected, |body| body.key_context("Selection"))
                        .flex_1()
                        .w_full()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .when(results_text_selected, |body| {
                                    body.key_context("TextSelected")
                                })
                                .track_focus(&self.results_focus)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        this.focus_pane(Pane::Results, window, cx)
                                    }),
                                )
                                .flex_1()
                                .w_full()
                                .min_h_0()
                                .flex()
                                .flex_col()
                                .when(result_count == 0, |col| {
                                    col.child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(t.small_size)
                                            .text_color(t.muted)
                                            .child(empty_results),
                                    )
                                })
                                .when(result_count > 0, |col| {
                                    col.child(
                                        div()
                                            .flex_1()
                                            .w_full()
                                            .min_h_0()
                                            .flex()
                                            .child(
                                                div()
                                                    .relative()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .h_full()
                                                    .child(
                                                        uniform_list(
                                                            "results",
                                                            result_count,
                                                            cx.processor(
                                                                |this, range, window, cx| {
                                                                    this.rows(
                                                                        range, true, window, cx,
                                                                    )
                                                                },
                                                            ),
                                                        )
                                                        .with_horizontal_sizing_behavior(
                                                            ListHorizontalSizingBehavior::Unconstrained,
                                                        )
                                                        .with_width_from_item(Some(widest_result))
                                                        .track_scroll(&result_scroll)
                                                        .size_full(),
                                                    )
                                                    .child(self.render_horizontal_scrollbar(
                                                        Pane::Results,
                                                        result_scroll.clone(),
                                                        "results-hscroll",
                                                        cx,
                                                    )),
                                            )
                                            .child(self.render_vertical_scrollbar(
                                                Pane::Results,
                                                result_scroll.clone(),
                                                true,
                                                DEFAULT_SCROLLBAR_WIDTH,
                                                "results-vscroll",
                                                cx,
                                            )),
                                    )
                                }),
                        ),
                );

            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_file_path_bar(
                    main_focused && !main_hidden,
                    main_hidden,
                    loading,
                    line_count,
                    window,
                    cx,
                ))
                .when(!main_hidden, |area| area.child(source_pane))
                .child(self.render_results_resize_sash(main_hidden, results_hidden, cx))
                .child(self.render_search_bar(&options, window, cx))
                .child(results_header)
                .when(!results_hidden, |area| area.child(results_body))
        };

        div()
            .size_full()
            .when(self.results_resize_drag.is_some(), |root| {
                root.cursor_row_resize()
            })
            .when(self.text_drag.is_some(), |root| root.cursor_text())
            .capture_key_up(cx.listener(|this, _event: &KeyUpEvent, _window, _cx| {
                this.resize_motion.release_hold();
            }))
            .on_modifiers_changed(cx.listener(
                |this, _event: &ModifiersChangedEvent, _window, _cx| {
                    this.resize_motion.release_hold();
                },
            ))
            .on_mouse_move_all({
                let view = cx.entity().downgrade();
                move |event: &MouseMoveEvent, phase, _hitbox, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = view.update(cx, |this, cx| {
                            this.handle_results_resize_move(event, window, cx);
                            this.handle_scrollbar_drag_move(event, cx);
                            this.handle_text_drag_move(event, cx);
                        });
                    }
                }
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag_selecting = None;
                    let mut changed = false;
                    if this.text_drag.take().is_some() {
                        changed = true;
                    }
                    if this.scrollbar_drag.take().is_some() {
                        changed = true;
                    }
                    if this.results_resize_drag.take().is_some() {
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag_selecting = None;
                    let mut changed = false;
                    if this.text_drag.take().is_some() {
                        changed = true;
                    }
                    if this.scrollbar_drag.take().is_some() {
                        changed = true;
                    }
                    if this.results_resize_drag.take().is_some() {
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                for path in paths.paths() {
                    this.open(path.clone(), cx);
                }
            }))
            .flex()
            .flex_col()
            .key_context(if regex_mode {
                "Workspace RegexMode"
            } else {
                "Workspace"
            })
            .bg(t.background)
            .text_color(t.foreground)
            .font(t.ui_font_spec())
            .text_size(t.ui_size)
            .on_action(cx.listener(|this, action: &Execute, window, cx| {
                this.command(action.command, window, cx)
            }))
            .child(self.render_header(window, cx))
            .when(self.show_files, |root| {
                root.child(self.render_file_drawer(cx))
            })
            .when_some(tab_error, |root, error| {
                root.child(self.render_error_banner(error, cx))
            })
            .child(main_area)
            .when(self.config.window.show_status_bar, |root| {
                root.child(self.render_status_bar(window, cx))
            })
    }
}

fn format_display_line(
    log: &LogFile,
    line: usize,
    max_display_chars: usize,
    tab_width: usize,
    highlight_regex: Option<&Regex>,
    search_mode: SearchMode,
) -> (String, usize, Vec<std::ops::Range<usize>>) {
    match log.line(line, max_display_chars.saturating_mul(4)) {
        Ok(raw) => {
            let mut chars = raw.chars();
            let display_source: String = chars.by_ref().take(max_display_chars).collect();
            let truncated = chars.next().is_some();
            let raw_ranges = highlight_regex
                .map(|regex| search::highlight_ranges(&display_source, regex, search_mode))
                .unwrap_or_default();
            let (mut text, mapped_ranges) =
                search::expand_tabs_with_highlights(&display_source, &raw_ranges, tab_width);
            let selectable_len = text.len();
            if truncated {
                text.push_str(" … [truncated]");
            }
            (text, selectable_len, mapped_ranges)
        }
        Err(e) => {
            let text = format!("Read error: {e}");
            let len = text.len();
            (text, len, Vec::new())
        }
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

fn parent_label(path: &Path) -> String {
    path.parent()
        .map(|parent| parent.display().to_string())
        .unwrap_or_default()
}

pub fn run(config: Config, paths: Vec<PathBuf>) -> anyhow::Result<()> {
    let bindings = keybindings(&config)?;
    gpui_platform::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx: &mut App| {
            // GPUI CE 0.3.0 invokes this callback while holding the macOS
            // platform mutex. Opening a native window here takes that mutex
            // again. Schedule startup after the launch callback has returned.
            cx.spawn(async move |cx| {
                cx.update(move |cx| {
                    cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
                    cx.bind_keys(bindings);
                    let bounds = Bounds::centered(
                        None,
                        size(px(config.window.width), px(config.window.height)),
                        cx,
                    );
                    let bounds = if config.window.maximized {
                        WindowBounds::Maximized(bounds)
                    } else {
                        WindowBounds::Windowed(bounds)
                    };
                    let options = WindowOptions {
                        window_bounds: Some(bounds),
                        window_min_size: Some(size(px(640.), px(480.))),
                        titlebar: Some(TitlebarOptions {
                            title: Some(config.window.title.clone().into()),
                            appears_transparent: true,
                            traffic_light_position: Some(point(px(14.), px(16.))),
                        }),
                        ..Default::default()
                    };
                    if let Err(error) = cx.open_window(options, |window, cx| {
                        cx.new(|cx| Workspace::new(config, paths, window, cx))
                    }) {
                        eprintln!("Cannot open window: {error}");
                        cx.quit();
                    }
                    cx.activate(true);
                });
            })
            .detach();
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[gpui::test]
    fn search_field_does_not_run_log_shortcuts(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, _| view.tabs[0].log = Some(log));
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx)
        });
        cx.simulate_input("failure message");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "failure message")
        });
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].query, "failure message");
            assert!(view.tabs[0].bookmarks.is_empty());
            assert!(!view.tabs[0].follow);
        });
        cx.simulate_keystrokes("escape");
        view.update_in(cx, |view, window, _cx| {
            assert!(view.focus.is_focused(window))
        });
        cx.simulate_keystrokes("m");
        view.read_with(cx, |view, _| assert!(view.tabs[0].bookmarks.contains(&0)));
        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx)
        });
        cx.simulate_input("3");
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 2);
            assert!(!view.show_goto);
        });
    }

    #[gpui::test]
    fn search_option_shortcuts_toggle_only_when_search_is_focused(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("search_opts.log");
        std::fs::write(&path, "ERROR one\nerror two\nterror three\na.b\naxb\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, _| view.tabs[0].log = Some(log));

        let stroke = |key: &str| {
            if cfg!(target_os = "macos") {
                format!("cmd-alt-{key}")
            } else {
                format!("ctrl-alt-{key}")
            }
        };

        // Initial defaults from config/search.json: Regex, case_sensitive/whole_word/invert off.
        view.read_with(cx, |view, _| {
            let opts = &view.tabs[0].options;
            assert_eq!(opts.mode, SearchMode::Regex);
            assert!(!opts.case_sensitive);
            assert!(!opts.whole_word);
            assert!(!opts.invert);
            let expected = |key: &str| {
                if cfg!(target_os = "macos") {
                    format!("⌘⌥{key}")
                } else {
                    format!("Ctrl+Alt+{key}")
                }
            };
            assert_eq!(
                view.theme.shortcut(Command::ToggleRegex).as_deref(),
                Some(expected("R").as_str())
            );
            assert_eq!(
                view.theme.shortcut(Command::ToggleCaseSensitive).as_deref(),
                Some(expected("C").as_str())
            );
            assert_eq!(
                view.theme.shortcut(Command::ToggleWholeWord).as_deref(),
                Some(expected("W").as_str())
            );
            assert_eq!(
                view.theme.shortcut(Command::ToggleInvert).as_deref(),
                Some(expected("I").as_str())
            );
        });

        // While the log pane is focused (Search is not focused), none of the search option shortcuts fire.
        cx.simulate_keystrokes(&stroke("r"));
        cx.simulate_keystrokes(&stroke("c"));
        cx.simulate_keystrokes(&stroke("w"));
        cx.simulate_keystrokes(&stroke("i"));
        view.read_with(cx, |view, _| {
            let opts = &view.tabs[0].options;
            assert_eq!(opts.mode, SearchMode::Regex);
            assert!(!opts.case_sensitive);
            assert!(!opts.whole_word);
            assert!(!opts.invert);
        });

        // Focus search with an empty query: toggling options does not start a background search.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx)
        });
        cx.simulate_keystrokes(&stroke("r"));
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
            assert!(!view.tabs[0].searching);
            assert!(!view.tabs[0].searching_visible);
        });
        cx.simulate_keystrokes(&stroke("r"));

        // Type a query and toggle each search option via keyboard.
        cx.simulate_input("a.b");

        cx.simulate_keystrokes(&stroke("r"));
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
            assert_eq!(view.tabs[0].query, "a.b");
            assert_eq!(view.input.read(cx).as_str(), "a.b");
            assert!(!view.tabs[0].searching_visible);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].searching);
            assert!(!view.tabs[0].searching_visible);
            assert_eq!(view.tabs[0].results.matches, vec![3]);
        });

        cx.simulate_keystrokes(&stroke("c"));
        view.read_with(cx, |view, cx| {
            assert!(view.tabs[0].options.case_sensitive);
            assert_eq!(view.input.read(cx).as_str(), "a.b");
            assert!(!view.tabs[0].searching_visible);
        });

        cx.simulate_keystrokes(&stroke("w"));
        view.read_with(cx, |view, cx| {
            assert!(view.tabs[0].options.whole_word);
            assert_eq!(view.input.read(cx).as_str(), "a.b");
            assert!(!view.tabs[0].searching_visible);
        });

        cx.simulate_keystrokes(&stroke("i"));
        view.read_with(cx, |view, cx| {
            assert!(view.tabs[0].options.invert);
            assert_eq!(view.input.read(cx).as_str(), "a.b");
            assert!(!view.tabs[0].searching_visible);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].results.matches, vec![0, 1, 2, 4]);
        });

        // Focus stays in the search input and pressing each shortcut again toggles them back.
        view.update_in(cx, |view, window, cx| {
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
        });
        cx.simulate_keystrokes(&stroke("r"));
        cx.simulate_keystrokes(&stroke("c"));
        cx.simulate_keystrokes(&stroke("w"));
        cx.simulate_keystrokes(&stroke("i"));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let opts = &view.tabs[0].options;
            assert_eq!(opts.mode, SearchMode::Regex);
            assert!(!opts.case_sensitive);
            assert!(!opts.whole_word);
            assert!(!opts.invert);
            assert_eq!(view.tabs[0].results.matches, vec![3, 4]);
        });

        // Toggling a bookmark updates visible rows synchronously without triggering a search
        // or consuming unsubmitted text from the search box.
        cx.simulate_input("extra");
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("m");
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].searching);
            assert!(!view.tabs[0].searching_visible);
            assert_eq!(view.tabs[0].query, "a.b");
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
            assert_eq!(view.tabs[0].results.visible, vec![0, 3, 4]);
        });
    }

    #[gpui::test]
    fn all_icons_rasterize_with_visible_pixels(cx: &mut TestAppContext) {
        cx.update(|cx| {
            for icon in Icon::ALL {
                let image = cx
                    .svg_renderer()
                    .render_single_frame(icon.svg_bytes(), 2.0)
                    .unwrap_or_else(|err| panic!("Icon::{icon:?} failed SVG rendering: {err}"));
                let has_visible_pixel = image
                    .as_bytes(0)
                    .expect("icon frame bytes")
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|px| px[3] > 0);
                assert!(
                    has_visible_pixel,
                    "Icon::{icon:?} rasterized completely transparent"
                );
            }
        });
    }

    #[test]
    fn resolves_preferred_fonts_and_formats_telemetry() {
        let installed = vec![
            "Menlo".to_owned(),
            "SauceCodePro Nerd Font Mono".to_owned(),
            "SF Pro Text".to_owned(),
        ];
        assert_eq!(
            theme::resolve_font("monospace", &["JetBrains Mono", "Menlo"], &installed),
            "Menlo"
        );
        assert_eq!(
            theme::resolve_font(
                "JetBrains Mono",
                &["JetBrains Mono", "SauceCodePro Nerd Font Mono", "Menlo"],
                &installed,
            ),
            "SauceCodePro Nerd Font Mono"
        );
        assert_eq!(
            theme::resolve_font("MissingFont", &["JetBrains Mono", "Menlo"], &installed),
            "Menlo"
        );
        assert_eq!(
            theme::resolve_font("SF Pro Text", &["Inter", "SF Pro Text"], &installed),
            "SF Pro Text"
        );
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_count(12480), "12,480");
    }

    #[gpui::test]
    fn closing_tabs_and_clearing_search_keep_state_consistent(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        std::fs::write(&first, "alpha\n").unwrap();
        std::fs::write(&second, "beta\n").unwrap();

        let (view, cx) = cx
            .add_window_view(|window, cx| Workspace::new(config, vec![first, second], window, cx));

        view.update(cx, |view, cx| {
            view.input.update(cx, |input, cx| input.emplace("beta", cx));
            view.close_tab_at(0, cx);
        });
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs.len(), 1);
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "beta");
        });

        view.update(cx, |view, cx| view.clear_search(cx));
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "");
            assert_eq!(view.tabs[0].query, "");
        });
    }

    #[test]
    fn invalid_keybindings_are_errors_instead_of_panics() {
        let mut config = Config::load(None).unwrap();
        config.keyboard.bindings[0].keys = "invalidmodifier-o".into();
        assert!(keybindings(&config).is_err());
    }

    #[gpui::test]
    fn goto_popover_sits_below_file_path_without_shifting_or_wrapping(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, _| view.tabs[0].log = Some(log));
        cx.run_until_parked();

        let file_path_before = cx
            .debug_bounds("file-path-bar")
            .expect("file path bar should be rendered");
        assert!(cx.debug_bounds("goto-popover").is_none());

        view.update_in(cx, |view, window, cx| view.toggle_goto(window, cx));
        cx.run_until_parked();

        let file_path_after = cx
            .debug_bounds("file-path-bar")
            .expect("file path bar should remain rendered");
        let popover = cx
            .debug_bounds("goto-popover")
            .expect("goto popover should be rendered");
        let input_bounds = cx
            .debug_bounds("goto-input")
            .expect("goto input should be rendered");

        assert_eq!(
            file_path_before.origin.y, file_path_after.origin.y,
            "opening go-to-line should not shift the file path bar"
        );
        assert!(
            popover.origin.y >= file_path_after.bottom(),
            "go-to-line popover ({:?}) should appear below the file path bar ({:?})",
            popover,
            file_path_after
        );
        assert!(
            input_bounds.size.height <= px(28.),
            "goto placeholder should stay single-line, got height {:?}",
            input_bounds.size.height
        );

        // Repeated redraws must not alternate between wrapped and single-line layout.
        for _ in 0..5 {
            cx.refresh().unwrap();
            let redrawn = cx
                .debug_bounds("goto-input")
                .expect("goto input should stay rendered");
            assert_eq!(
                redrawn.size.height, input_bounds.size.height,
                "goto input height should remain stable across redraws"
            );
        }

        // Typing log shortcut letters inside the popover must not trigger Log actions.
        cx.simulate_input("mf");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.goto.read(cx).as_str(), "mf");
            assert!(view.tabs[0].bookmarks.is_empty());
            assert!(!view.tabs[0].follow);
        });

        // Toggling go-to-line closed should dismiss without showing an invalid-number error.
        view.update_in(cx, |view, window, cx| view.toggle_goto(window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.show_goto);
            assert!(view.status.is_none());
        });
        assert!(cx.debug_bounds("goto-popover").is_none());

        // Multi-word queries in the search input must also stay single-line across redraws.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx)
        });
        cx.simulate_input("connection refused timeout");
        cx.run_until_parked();
        let search_bounds = cx
            .debug_bounds("search-input")
            .expect("search input should be rendered");
        assert!(
            search_bounds.size.height <= px(34.),
            "search input should stay single-line, got height {:?}",
            search_bounds.size.height
        );
        for _ in 0..5 {
            cx.refresh().unwrap();
            let redrawn = cx
                .debug_bounds("search-input")
                .expect("search input should stay rendered");
            assert_eq!(redrawn.size.height, search_bounds.size.height);
        }
    }

    #[gpui::test]
    fn main_pane_supports_keyboard_and_mouse_multi_selection_copy_and_bookmark(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("multi.log");
        std::fs::write(&path, "zero\none\ntwo\nthree\nfour\nfive\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].main_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        // Keyboard Shift+Down extends contiguous selection in the main pane.
        cx.simulate_keystrokes("down shift-down shift-down");
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.tabs[0].main_selection.iter().collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
            assert_eq!(view.tabs[0].selected, 3);
        });

        // Copy copies all selected lines in order.
        let copy_key = if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-c"
        };
        let select_all_key = if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        };
        cx.simulate_keystrokes(copy_key);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain copied lines");
        assert_eq!(copied, "one\ntwo\nthree");

        // Bookmarking toggles all selected lines together.
        cx.simulate_keystrokes("m");
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([1, 2, 3]));
        });
        cx.simulate_keystrokes("m");
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].bookmarks.is_empty());
        });

        // Mouse Shift+Click, Secondary+Click, and Click+Drag in main pane.
        let row1 = cx.debug_bounds("main-row-1").expect("main row 1").center();
        let row3 = cx.debug_bounds("main-row-3").expect("main row 3").center();
        let row5 = cx.debug_bounds("main-row-5").expect("main row 5").center();

        cx.simulate_click(row1, Modifiers::none());
        cx.simulate_click(row3, Modifiers::shift());
        cx.simulate_click(row5, Modifiers::secondary_key());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.tabs[0].main_selection.iter().collect::<Vec<_>>(),
                vec![1, 2, 3, 5]
            );
        });

        // Click-and-drag from row 1 to row 3 selects 1..=3.
        cx.simulate_mouse_down(row1, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(row3, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(row3, MouseButton::Left, Modifiers::none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.tabs[0].main_selection.iter().collect::<Vec<_>>(),
                vec![1, 2, 3]
            );
        });

        // Select All and Escape collapse.
        cx.simulate_keystrokes(select_all_key);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.count(), 6);
        });
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.count(), 1);
        });
    }

    #[gpui::test]
    fn filter_pane_keeps_focus_navigates_filtered_rows_and_supports_multi_selection(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("filter.log");
        std::fs::write(
            &path,
            "info 0\nERROR one\ninfo 2\ninfo 3\nERROR two\ninfo 5\nERROR three\nERROR four\n",
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let results = SearchResults {
            matches: vec![1, 4, 6, 7],
            visible: vec![1, 4, 6, 7],
            limited: false,
            widest_row: 0,
        };
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].query = "ERROR".into();
            view.tabs[0].results = results;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        // Clicking a filtered row keeps keyboard focus in the filter pane and syncs the main pane.
        let res0 = cx
            .debug_bounds("result-row-0")
            .expect("result row 0")
            .center();
        cx.simulate_click(res0, Modifiers::none());
        view.update_in(cx, |view, window, _| {
            assert!(
                view.results_focus.is_focused(window),
                "clicking a result row should keep focus in the filter pane"
            );
            assert_eq!(view.tabs[0].result_selection.cursor, 0);
            assert_eq!(view.tabs[0].selected, 1);
        });

        // Pressing Down moves to the next filtered row (source line 4), NOT line 2 in the main pane.
        cx.simulate_keystrokes("down");
        view.update_in(cx, |view, window, _| {
            assert!(view.results_focus.is_focused(window));
            assert_eq!(view.tabs[0].result_selection.cursor, 1);
            assert_eq!(view.tabs[0].selected, 4);
        });

        // Shift+Down multi-selects only filtered rows 1..=2 (source lines 4 and 6).
        cx.simulate_keystrokes("shift-down");
        let copy_key = if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-c"
        };
        cx.simulate_keystrokes(copy_key);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain copied filtered lines");
        assert_eq!(copied, "ERROR two\nERROR three");

        // Secondary+Click toggles non-contiguous filtered rows (add row 0 -> source lines 1, 4, 6).
        cx.simulate_click(res0, Modifiers::secondary_key());
        cx.simulate_keystrokes(copy_key);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain non-contiguous filtered lines");
        assert_eq!(copied, "ERROR one\nERROR two\nERROR three");

        // Pressing m bookmarks all selected filtered lines.
        cx.simulate_keystrokes("m");
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([1, 4, 6]));
        });

        // Pressing Tab switches focus between filter pane and main pane.
        cx.simulate_keystrokes("tab");
        view.update_in(cx, |view, window, _| {
            assert!(
                view.focus.is_focused(window),
                "Tab from filter pane should focus main pane"
            );
        });
        cx.simulate_keystrokes("tab");
        view.update_in(cx, |view, window, _| {
            assert!(
                view.results_focus.is_focused(window),
                "Tab from main pane should focus filter pane"
            );
        });
    }

    #[gpui::test]
    fn saved_patterns_dropdown_supports_keyboard_and_mouse_workflows(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presets.log");
        std::fs::write(
            &path,
            "info boot\nWARN slow query\nERROR panic\nGET /health\n",
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].main_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        let presets_key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };

        // Dropdown trigger is visible while menu is initially closed.
        let trigger_bounds = cx
            .debug_bounds("presets-trigger")
            .expect("presets trigger should be rendered");
        assert!(cx.debug_bounds("presets-menu").is_none());

        // Keyboard shortcut opens dropdown, auto-focuses its search box, and positions it cleanly below trigger.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.show_presets);
            assert!(view.presets_search_focused(window, cx));
            assert_eq!(view.preset_index, 0);
        });
        let menu_bounds = cx
            .debug_bounds("presets-menu")
            .expect("presets menu should be rendered when open");
        assert!(
            menu_bounds.origin.y >= trigger_bounds.bottom(),
            "presets menu ({menu_bounds:?}) should open below trigger ({trigger_bounds:?})"
        );

        // Arrow navigation inside dropdown moves highlight without moving log selection.
        cx.simulate_keystrokes("down");
        view.read_with(cx, |view, _| {
            assert_eq!(view.preset_index, 1);
            assert_eq!(view.tabs[0].selected, 0);
        });
        cx.simulate_keystrokes("end");
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 2));
        cx.simulate_keystrokes("down");
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 0));
        cx.simulate_keystrokes("up");
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 2));
        cx.simulate_keystrokes("home down");
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 1));

        // Pressing Enter applies the highlighted preset ("Warnings": literal "warn"), closes menu, and restores focus.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.focus.is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "warn");
            assert_eq!(view.tabs[0].query, "warn");
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
        });
        assert!(cx.debug_bounds("presets-menu").is_none());
        let trigger_after_warnings = cx
            .debug_bounds("presets-trigger")
            .expect("presets trigger after applying Warnings");
        assert_eq!(
            trigger_after_warnings.size.width, trigger_bounds.size.width,
            "presets trigger width should remain constant when switching presets"
        );
        assert_eq!(
            trigger_after_warnings.origin.x, trigger_bounds.origin.x,
            "presets trigger position should remain constant when switching presets"
        );

        // Reopening highlights the active preset and Escape closes without changing query.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_presets);
            assert_eq!(view.preset_index, 1);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.focus.is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "warn");
        });

        // Shortcut also works from the search input and toggles closed on second press.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx)
        });
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.show_presets));
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(!view.show_presets));

        // Clicking the trigger opens the dropdown, and clicking it again closes it.
        let trigger_center = cx
            .debug_bounds("presets-trigger")
            .expect("presets trigger")
            .center();
        cx.simulate_click(trigger_center, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.show_presets));

        cx.simulate_click(trigger_center, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(!view.show_presets));

        // Clicking a preset item applies it and closes the dropdown.
        cx.simulate_click(trigger_center, Modifiers::none());
        cx.run_until_parked();
        let item2_center = cx
            .debug_bounds("preset-item-2")
            .expect("third preset item")
            .center();
        cx.simulate_click(item2_center, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.show_presets);
            assert_eq!(view.input.read(cx).as_str(), "GET|POST|PUT|DELETE");
            assert_eq!(view.tabs[0].options.mode, SearchMode::Regex);
        });

        // Clicking outside the open dropdown dismisses it.
        cx.simulate_click(trigger_center, Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("presets-menu").is_some());
        let row0 = cx.debug_bounds("main-row-0").expect("main row 0").center();
        cx.simulate_click(row0, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(!view.show_presets));
        assert!(cx.debug_bounds("presets-menu").is_none());
        let trigger_after_http = cx
            .debug_bounds("presets-trigger")
            .expect("presets trigger after applying third preset");
        assert_eq!(
            trigger_after_http.size.width, trigger_bounds.size.width,
            "presets trigger width should remain constant when switching presets"
        );
        assert_eq!(
            trigger_after_http.origin.x, trigger_bounds.origin.x,
            "presets trigger position should remain constant when switching presets"
        );
    }

    #[gpui::test]
    fn applying_preset_updates_highlight_matches_and_active_preset_matching(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.search.presets[0].highlight_match = false;
        config.search.presets[1].highlight_match = true;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("preset-highlight.log");
        std::fs::write(&path, "info boot\nWARN slow query\nERROR panic\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].main_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        // Initially highlight_matches is true from SearchConfig defaults.
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].options.highlight_matches);
        });

        // Applying preset 0 (highlight_match = false) disables match highlighting and matches preset 0.
        view.update_in(cx, |view, window, cx| view.apply_preset(0, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs[0].query, "error|fatal|panic");
            assert_eq!(view.tabs[0].options.mode, SearchMode::Regex);
            assert!(!view.tabs[0].options.highlight_matches);
            assert_eq!(view.active_preset_index(cx), Some(0));
        });

        // Toggling match highlighting manually detaches the active preset match.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::ToggleHighlightMatches, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.tabs[0].options.highlight_matches);
            assert_eq!(view.active_preset_index(cx), None);
        });

        // Re-applying preset 0 turns highlighting back off; applying preset 1 (highlight_match = true) turns it on.
        view.update_in(cx, |view, window, cx| view.apply_preset(0, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.tabs[0].options.highlight_matches);
            assert_eq!(view.active_preset_index(cx), Some(0));
        });

        view.update_in(cx, |view, window, cx| view.apply_preset(1, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs[0].query, "warn");
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
            assert!(view.tabs[0].options.highlight_matches);
            assert_eq!(view.active_preset_index(cx), Some(1));
        });
    }

    #[gpui::test]
    fn saved_patterns_fuzzy_search_live_filters_and_picks_on_enter(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.search.presets = vec![
            config::Preset {
                name: "Errors".into(),
                pattern: "error|fatal|panic".into(),
                mode: SearchMode::Regex,
                highlight_match: true,
            },
            config::Preset {
                name: "Warnings".into(),
                pattern: "warn".into(),
                mode: SearchMode::Literal,
                highlight_match: true,
            },
            config::Preset {
                name: "Requests".into(),
                pattern: "GET|POST|PUT|DELETE".into(),
                mode: SearchMode::Regex,
                highlight_match: true,
            },
            config::Preset {
                name: "HTTP 5xx Errors".into(),
                pattern: "HTTP 5\\d\\d".into(),
                mode: SearchMode::Regex,
                highlight_match: true,
            },
            config::Preset {
                name: "Auth Errors".into(),
                pattern: "401|403|unauthorized".into(),
                mode: SearchMode::Regex,
                highlight_match: false,
            },
        ];
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fuzzy-presets.log");
        std::fs::write(
            &path,
            "info boot\nWARN slow query\nERROR panic\nHTTP 502 bad gateway\n401 unauthorized\n",
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].main_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        let presets_key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };

        // Opening saved patterns focuses the fuzzy search input and keeps it single-line.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.show_presets);
            assert!(view.presets_search_focused(window, cx));
            assert_eq!(view.filtered_presets.len(), 5);
            assert_eq!(view.preset_index, 0);
        });
        let input_bounds = cx
            .debug_bounds("presets-search-input")
            .expect("presets search input should render");
        assert!(
            input_bounds.size.height <= px(28.),
            "presets search input should stay single-line, got {:?}",
            input_bounds.size.height
        );
        for _ in 0..3 {
            cx.refresh().unwrap();
            let redrawn = cx
                .debug_bounds("presets-search-input")
                .expect("presets search input after redraw");
            assert_eq!(redrawn.size.height, input_bounds.size.height);
        }

        // Typing a fuzzy query immediately filters by name, highlights top match, and Enter picks it.
        cx.simulate_input("wrn");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.presets_search.read(cx).as_str(), "wrn");
            assert_eq!(
                view.filtered_presets
                    .iter()
                    .map(|m| m.index)
                    .collect::<Vec<_>>(),
                vec![1]
            );
            assert_eq!(view.preset_index, 0);
        });
        assert!(cx.debug_bounds("preset-item-1").is_some());
        assert!(cx.debug_bounds("preset-item-0").is_none());

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.focus.is_focused(window));
            assert_eq!(view.presets_search.read(cx).as_str(), "");
            assert_eq!(view.tabs[0].query, "warn");
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
        });

        // Reopening starts with an empty filter and highlights the active preset ("Warnings" = 1).
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.show_presets);
            assert_eq!(view.presets_search.read(cx).as_str(), "");
            assert_eq!(view.filtered_presets.len(), 5);
            assert_eq!(view.preset_index, 1);
        });

        // Multi-word fuzzy query with space keystroke must insert space (not apply preset) and match "HTTP 5xx Errors".
        cx.simulate_input("http");
        cx.simulate_keystrokes("space");
        cx.simulate_input("err");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.show_presets);
            assert_eq!(view.presets_search.read(cx).as_str(), "http err");
            assert_eq!(
                view.filtered_presets
                    .iter()
                    .map(|m| m.index)
                    .collect::<Vec<_>>(),
                vec![3]
            );
            assert_eq!(view.preset_index, 0);
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.show_presets);
            assert_eq!(view.tabs[0].query, "HTTP 5\\d\\d");
        });

        // Typing "err" matches multiple presets ranked best-first; arrows pick the second match.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        cx.simulate_input("err");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let ranked: Vec<usize> = view.filtered_presets.iter().map(|m| m.index).collect();
            assert_eq!(ranked[0], 0, "exact prefix 'Errors' should rank first");
            assert_eq!(ranked.len(), 3);
            assert_eq!(view.preset_index, 0);
        });
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let expected_second_index = view.read_with(cx, |view, _| {
            assert_eq!(view.preset_index, 1);
            view.filtered_presets[1].index
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.show_presets);
            assert_eq!(
                view.tabs[0].query,
                view.config.search.presets[expected_second_index].pattern
            );
        });

        // Pattern body text ("panic") is NOT matched when searching by pattern name.
        let query_before_no_match = view.read_with(cx, |view, _| view.tabs[0].query.clone());
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        cx.simulate_input("panic");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_presets);
            assert!(view.filtered_presets.is_empty());
        });
        assert!(cx.debug_bounds("presets-empty").is_some());

        // Enter and navigation on empty results are safe no-ops and keep the dropdown open.
        cx.simulate_keystrokes("down up home end enter");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_presets);
            assert!(view.filtered_presets.is_empty());
            assert_eq!(view.tabs[0].query, query_before_no_match);
        });

        // Clicking the inline clear button restores all matches and keeps focus in the search input.
        let clear_center = cx
            .debug_bounds("clear-presets-search")
            .expect("clear presets search button should render")
            .center();
        cx.simulate_click(clear_center, Modifiers::none());
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.show_presets);
            assert!(view.presets_search_focused(window, cx));
            assert_eq!(view.presets_search.read(cx).as_str(), "");
            assert_eq!(view.filtered_presets.len(), 5);
            assert_eq!(view.preset_index, 0);
        });

        // Filtering and clicking a filtered item directly applies it and clears the filter.
        cx.simulate_input("au");
        cx.run_until_parked();
        let auth_center = cx
            .debug_bounds("preset-item-4")
            .expect("Auth Errors item should render")
            .center();
        cx.simulate_click(auth_center, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.show_presets);
            assert_eq!(view.presets_search.read(cx).as_str(), "");
            assert_eq!(view.tabs[0].query, "401|403|unauthorized");
            assert!(!view.tabs[0].options.highlight_matches);
        });

        // Escape dismisses and clears any typed filter query.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        cx.simulate_input("req");
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.focus.is_focused(window));
            assert_eq!(view.presets_search.read(cx).as_str(), "");
        });
    }

    #[test]
    fn theme_fallbacks_include_symbol_fonts_without_duplicates() {
        let config = Config::load(None).unwrap();
        let installed = vec![
            "SF Pro Text".to_owned(),
            "SauceCodePro Nerd Font Mono".to_owned(),
            "Menlo".to_owned(),
            "Apple Symbols".to_owned(),
            "Lucida Grande".to_owned(),
        ];
        let theme = Theme::from_config_with_fonts(&config, &installed);
        assert_eq!(theme.ui_font.as_ref(), "SF Pro Text");
        assert_eq!(theme.mono_font.as_ref(), "SauceCodePro Nerd Font Mono");
        assert_eq!(
            theme.ui_fallbacks.fallback_list(),
            &["Apple Symbols", "Lucida Grande", "Menlo"]
        );
        assert_eq!(
            theme.mono_fallbacks.fallback_list(),
            &["Menlo", "Apple Symbols", "Lucida Grande"]
        );
        assert_eq!(
            theme
                .mono_font_spec()
                .fallbacks
                .as_ref()
                .map(|fallbacks| fallbacks.fallback_list()),
            Some(
                &[
                    "Menlo".to_owned(),
                    "Apple Symbols".to_owned(),
                    "Lucida Grande".to_owned()
                ][..]
            )
        );
    }

    #[test]
    fn status_message_tones_use_expected_icons_and_colors() {
        let config = Config::load(None).unwrap();
        let theme = Theme::from_config_with_fonts(&config, &[]);

        assert_eq!(
            StatusMessage::success("Copied Ln 1").icon_and_color(&theme),
            (Icon::Check, theme.accent)
        );
        assert_eq!(
            StatusMessage::warning("Invalid line number").icon_and_color(&theme),
            (Icon::AlertTriangle, theme.warning)
        );
        assert_eq!(
            StatusMessage::error("Export failed").icon_and_color(&theme),
            (Icon::AlertTriangle, theme.danger)
        );
    }

    #[gpui::test]
    fn invalid_and_out_of_range_goto_line_sets_warning_status(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("goto-status.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, _| view.tabs[0].log = Some(log));
        cx.run_until_parked();

        view.update_in(cx, |view, window, cx| view.toggle_goto(window, cx));
        cx.simulate_input("abc");
        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::warning("Invalid line number"))
            );
            assert!(view.show_goto);
        });

        view.update_in(cx, |view, window, cx| {
            view.goto.update(cx, |input, cx| input.emplace("99", cx));
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.status, Some(StatusMessage::warning("Range: 1–3")));
            assert!(view.show_goto);
        });

        view.update_in(cx, |view, window, cx| {
            view.goto.update(cx, |input, cx| input.emplace("2", cx));
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.status, None);
            assert!(!view.show_goto);
            assert_eq!(view.tabs[0].selected, 1);
        });
    }

    #[gpui::test]
    fn alt_o_swaps_selection_anchor_in_main_and_filter_panes_using_selection_context(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("anchor.log");
        let content = (0..18)
            .map(|i| {
                if matches!(i, 2 | 5 | 8 | 11 | 14) {
                    format!("ERROR line {i}\n")
                } else {
                    format!("INFO line {i}\n")
                }
            })
            .collect::<String>();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let results = SearchResults {
            matches: vec![2, 5, 8, 11, 14],
            visible: vec![2, 5, 8, 11, 14],
            limited: false,
            widest_row: 0,
        };

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].query = "ERROR".into();
            view.tabs[0].results = results;
            view.tabs[0].main_selection = LineSelection::single(3);
            view.tabs[0].selected = 3;
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        // Single-line selection does not activate Selection context.
        cx.simulate_keystrokes("alt-o");
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.anchor, 3);
            assert_eq!(view.tabs[0].main_selection.cursor, 3);
            assert_eq!(view.tabs[0].selected, 3);
        });

        // Select rows 3..=12 (10 selected, cursor at bottom = 12, anchor at top = 3).
        view.update(cx, |view, cx| {
            view.tabs[0].main_selection.extend_to(12);
            view.tabs[0].selected = 12;
            cx.notify();
        });
        cx.run_until_parked();

        // Pressing alt-o flips anchor to bottom (12) and cursor to top (3).
        cx.simulate_keystrokes("alt-o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(3, 12)]);
            assert_eq!(view.tabs[0].main_selection.anchor, 12);
            assert_eq!(view.tabs[0].main_selection.cursor, 3);
            assert_eq!(view.tabs[0].selected, 3);
        });

        // Shift+Down shrinks from the top to rows 4..=12 (9 selected, cursor = 4, anchor = 12).
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(4, 12)]);
            assert_eq!(view.tabs[0].main_selection.count(), 9);
            assert_eq!(view.tabs[0].main_selection.anchor, 12);
            assert_eq!(view.tabs[0].main_selection.cursor, 4);
            assert_eq!(view.tabs[0].selected, 4);
        });

        // Pressing alt-o again flips back so cursor is at bottom (12) and anchor is at top (4).
        cx.simulate_keystrokes("alt-o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(4, 12)]);
            assert_eq!(view.tabs[0].main_selection.anchor, 4);
            assert_eq!(view.tabs[0].main_selection.cursor, 12);
            assert_eq!(view.tabs[0].selected, 12);
        });

        // Focusing Search does not run Selection bindings even while main pane has a multi-selection.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.anchor, 4);
            assert_eq!(view.tabs[0].main_selection.cursor, 12);
        });

        // Switch to filter pane (only 1 filtered row selected): alt-o must not affect main or filter selection.
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Results, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.anchor, 4);
            assert_eq!(view.tabs[0].main_selection.cursor, 12);
            assert_eq!(view.tabs[0].result_selection.anchor, 0);
            assert_eq!(view.tabs[0].result_selection.cursor, 0);
        });

        // Multi-select filtered rows 0..=3 (source lines 2, 5, 8, 11).
        cx.simulate_keystrokes("shift-down shift-down shift-down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(0, 3)]);
            assert_eq!(view.tabs[0].result_selection.anchor, 0);
            assert_eq!(view.tabs[0].result_selection.cursor, 3);
            assert_eq!(view.tabs[0].selected, 11);
        });

        // Pressing alt-o in the filter pane swaps anchor/cursor to top (row 0 -> source line 2).
        cx.simulate_keystrokes("alt-o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(0, 3)]);
            assert_eq!(view.tabs[0].result_selection.anchor, 3);
            assert_eq!(view.tabs[0].result_selection.cursor, 0);
            assert_eq!(view.tabs[0].selected, 2);
        });

        // Shift+Down shrinks filtered selection from the top to rows 1..=3 (source line 5 active).
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(1, 3)]);
            assert_eq!(view.tabs[0].result_selection.anchor, 3);
            assert_eq!(view.tabs[0].result_selection.cursor, 1);
            assert_eq!(view.tabs[0].selected, 5);
        });

        // Pressing alt-o again moves cursor back to bottom of filtered selection (row 3 -> source line 11).
        cx.simulate_keystrokes("alt-o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(1, 3)]);
            assert_eq!(view.tabs[0].result_selection.anchor, 1);
            assert_eq!(view.tabs[0].result_selection.cursor, 3);
            assert_eq!(view.tabs[0].selected, 11);
        });
    }

    #[gpui::test]
    fn selection_context_overrides_log_context_only_when_multiple_lines_are_selected(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        // Bind `o` to bookmark in Log, and to swap_selection_anchor in Selection.
        config.keyboard.bindings.push(config::Binding {
            keys: "o".into(),
            context: config::KeyContext::Log,
            action: Command::ToggleBookmark,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "o".into(),
            context: config::KeyContext::Selection,
            action: Command::SwapSelectionAnchor,
        });
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("precedence.log");
        std::fs::write(&path, "zero\none\ntwo\nthree\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].main_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        // With a single line selected, `Log` handles `o` (bookmark).
        cx.simulate_keystrokes("o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
        });

        // With multiple lines selected, `Selection` takes precedence over `Log` and swaps anchor.
        cx.simulate_keystrokes("shift-down shift-down");
        cx.run_until_parked();
        cx.simulate_keystrokes("o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
            assert_eq!(view.tabs[0].main_selection.anchor, 2);
            assert_eq!(view.tabs[0].main_selection.cursor, 0);
        });

        // After collapsing selection with Escape, `Log` handles `o` again (unbookmark line 0).
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.simulate_keystrokes("o");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].bookmarks.is_empty());
        });
    }

    #[gpui::test]
    fn vertical_scrollbar_indicates_scroll_position_and_supports_click_and_drag(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scroll.log");
        let content = (0..200)
            .map(|i| format!("INFO line {i}\n"))
            .collect::<String>();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            cx.notify();
        });
        cx.run_until_parked();

        let initial = view.read_with(cx, |view, _| {
            view.main_vscroll_state
                .layout()
                .expect("vertical scrollbar layout for 200-line log")
        });
        assert!(initial.thumb.start.abs() < 0.5);
        assert!(initial.thumb.length < f32::from(initial.track_bounds.size.height));

        // Jumping to the last line updates the thumb to the bottom on the same draw cycle.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::LastLine, window, cx);
        });
        cx.run_until_parked();
        let at_bottom = view.read_with(cx, |view, _| {
            view.main_vscroll_state
                .layout()
                .expect("vertical scrollbar layout at bottom")
        });
        let track_h = f32::from(at_bottom.track_bounds.size.height);
        assert!((at_bottom.thumb.start + at_bottom.thumb.length - track_h).abs() < 1.0);

        // Return to top, turn on follow, then click near the middle of the track.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FirstLine, window, cx);
            view.tabs[0].follow = true;
            cx.notify();
        });
        cx.run_until_parked();

        let track_bounds = cx
            .debug_bounds("main-vscroll")
            .expect("main vertical scrollbar bounds");
        let mid_point = point(track_bounds.center().x, track_bounds.center().y);
        cx.simulate_click(mid_point, Modifiers::none());
        cx.run_until_parked();

        let after_click = view.read_with(cx, |view, _| {
            assert!(
                !view.tabs[0].follow,
                "clicking scrollbar should disable follow"
            );
            assert!(view.scrollbar_drag.is_none());
            let offset_y = f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(offset_y < -100.0, "expected scroll down, got {offset_y}");
            view.main_vscroll_state
                .layout()
                .expect("layout after track click")
        });
        assert!(after_click.thumb.start > 10.0);

        // Grab the thumb and drag it down to the bottom of the track.
        let thumb_grab = point(
            after_click.track_bounds.center().x,
            after_click.track_bounds.origin.y
                + px(after_click.thumb.start + after_click.thumb.length * 0.5),
        );
        let drag_target = point(
            after_click.track_bounds.center().x,
            after_click.track_bounds.bottom(),
        );
        cx.simulate_mouse_down(thumb_grab, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.scrollbar_drag.is_some()));

        cx.simulate_mouse_move(drag_target, Some(MouseButton::Left), Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_up(drag_target, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert!(view.scrollbar_drag.is_none());
            let layout = view.main_vscroll_state.layout().expect("layout after drag");
            let end_gap = f32::from(layout.track_bounds.size.height)
                - (layout.thumb.start + layout.thumb.length);
            assert!(
                end_gap.abs() < 1.5,
                "expected thumb near bottom after drag, end_gap={end_gap}"
            );
        });

        // Short file that fits inside the viewport should not show a vertical thumb.
        let short_path = directory.path().join("short.log");
        std::fs::write(&short_path, "one\ntwo\nthree\n").unwrap();
        let short_log = Arc::new(LogFile::open(&short_path, &AtomicBool::new(false)).unwrap());
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(short_log);
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.main_vscroll_state.layout().is_none());
        });
    }

    #[gpui::test]
    fn overview_marker_click_selects_line_without_starting_scrollbar_drag(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("markers.log");
        let content = (0..200)
            .map(|i| format!("INFO line {i}\n"))
            .collect::<String>();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].bookmarks.insert(150);
            cx.notify();
        });
        cx.run_until_parked();

        let overview_bounds = cx
            .debug_bounds("main-overview")
            .expect("overview lane bounds");
        let vscroll_bounds = cx
            .debug_bounds("main-vscroll")
            .expect("vertical scrollbar bounds");
        assert!(
            overview_bounds.right() <= vscroll_bounds.left(),
            "overview marker lane must sit to the left of the vertical scrollbar: overview={overview_bounds:?}, vscroll={vscroll_bounds:?}"
        );

        let marker_center = cx
            .debug_bounds("overview-marker-150")
            .expect("bookmark overview marker")
            .center();
        cx.simulate_click(marker_center, Modifiers::none());
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 150);
            assert!(view.scrollbar_drag.is_none());
            let layout = view
                .main_vscroll_state
                .layout()
                .expect("vertical scrollbar layout");
            assert!(layout.thumb.start > 0.0);
        });
    }

    #[gpui::test]
    fn horizontal_and_results_scrollbars_work_and_vertical_shows_without_overview(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.show_overview = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wide.log");
        let long_payload = "x".repeat(320);
        let content = (0..120)
            .map(|i| format!("MATCH line {i} {long_payload}\n"))
            .collect::<String>();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let visible: Vec<usize> = (0..120).collect();
        let results = SearchResults {
            matches: visible.clone(),
            visible,
            limited: false,
            widest_row: 0,
        };

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].query = "MATCH".into();
            view.tabs[0].results = results;
            view.tabs[0].selected = 0;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert!(
                view.main_vscroll_state.layout().is_some(),
                "main vertical scrollbar should appear even when show_overview is false"
            );
            assert!(
                view.main_hscroll_state.layout().is_some(),
                "main horizontal scrollbar should appear for wide lines"
            );
            assert!(
                view.results_vscroll_state.layout().is_some(),
                "results vertical scrollbar should appear when results overflow"
            );
            assert!(
                view.results_hscroll_state.layout().is_some(),
                "results horizontal scrollbar should appear for wide results"
            );
        });

        // Clicking the main horizontal scrollbar scrolls horizontally without changing selected line.
        let hbounds = cx
            .debug_bounds("main-hscroll")
            .expect("main horizontal scrollbar bounds");
        let right_click = point(
            hbounds.origin.x + hbounds.size.width * 0.85,
            hbounds.center().y,
        );
        cx.simulate_click(right_click, Modifiers::none());
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            let offset_x = f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x);
            assert!(
                offset_x < -50.0,
                "expected horizontal scroll, got {offset_x}"
            );
            assert_eq!(
                view.tabs[0].selected, 0,
                "clicking horizontal scrollbar must not select the row beneath it"
            );
            let hlayout = view
                .main_hscroll_state
                .layout()
                .expect("main horizontal scrollbar layout after click");
            assert!(hlayout.thumb.start > 10.0);
        });

        // Clicking the results vertical scrollbar scrolls the results pane.
        let rvbounds = cx
            .debug_bounds("results-vscroll")
            .expect("results vertical scrollbar bounds");
        let results_click = point(
            rvbounds.center().x,
            rvbounds.origin.y + rvbounds.size.height * 0.75,
        );
        cx.simulate_click(results_click, Modifiers::none());
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            let offset_y = f32::from(view.tabs[0].result_scroll.0.borrow().base_handle.offset().y);
            assert!(
                offset_y < -50.0,
                "expected results pane vertical scroll, got {offset_y}"
            );
        });
    }

    #[gpui::test]
    fn match_highlighting_toggle_sits_on_left_and_renders_regex_groups_and_literals(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.max_display_chars = 64;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("highlights.log");
        std::fs::write(
            &path,
            "INFO\tuser=alice\tlatency=124ms\nERROR\t(job #7842)\tשלום\t世界\nWARN\tlong line truncated suffix check 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\n",
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            cx.notify();
        });
        cx.run_until_parked();

        let highlight_key = if cfg!(target_os = "macos") {
            "cmd-alt-h"
        } else {
            "ctrl-alt-h"
        };
        let expected_shortcut = if cfg!(target_os = "macos") {
            "⌘⌥H"
        } else {
            "Ctrl+Alt+H"
        };

        // Highlighting starts enabled, registers its shortcut, and sits to the left of the search input.
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].options.highlight_matches);
            assert!(view.tabs[0].highlight_regex.is_none());
            assert_eq!(
                view.theme
                    .shortcut(Command::ToggleHighlightMatches)
                    .as_deref(),
                Some(expected_shortcut)
            );
        });
        let toggle_bounds = cx
            .debug_bounds("highlight-matches")
            .expect("highlight toggle should be rendered");
        let search_bounds = cx
            .debug_bounds("search-input")
            .expect("search input should be rendered");
        assert!(
            toggle_bounds.right() <= search_bounds.origin.x,
            "highlight toggle ({toggle_bounds:?}) should sit on the left side of the search bar before search input ({search_bounds:?})"
        );

        // Run a regex search with capturing groups: regex compiles and renders both panes cleanly.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.simulate_input(r"user=(\w+)\tlatency=(\d+ms)");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].results.matches, vec![0]);
            let regex = view.tabs[0]
                .highlight_regex
                .as_ref()
                .expect("compiled regex should be cached");
            assert_eq!(regex.captures_len(), 3);
        });
        cx.refresh().unwrap();

        // Clicking the left-side toggle disables highlighting without re-running search or altering unsubmitted text.
        cx.simulate_input(" unsubmitted");
        cx.simulate_click(toggle_bounds.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.tabs[0].options.highlight_matches);
            assert!(!view.tabs[0].searching);
            assert_eq!(
                view.input.read(cx).as_str(),
                r"user=(\w+)\tlatency=(\d+ms) unsubmitted"
            );
            assert_eq!(view.tabs[0].query, r"user=(\w+)\tlatency=(\d+ms)");
        });
        let toggle_after = cx
            .debug_bounds("highlight-matches")
            .expect("highlight toggle should remain rendered");
        assert_eq!(toggle_after.size, toggle_bounds.size);
        assert_eq!(toggle_after.origin, toggle_bounds.origin);

        // Keyboard shortcut toggles highlighting back on from both Search and Log focus.
        cx.simulate_keystrokes(highlight_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].options.highlight_matches);
        });
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes(highlight_key);
        cx.run_until_parked();
        view.update_in(cx, |view, window, _| {
            assert!(view.focus.is_focused(window));
            assert!(!view.tabs[0].options.highlight_matches);
        });
        cx.simulate_keystrokes(highlight_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].options.highlight_matches);
        });

        // Literal mode with regex metacharacters highlights the literal substring cleanly.
        view.update_in(cx, |view, window, cx| {
            view.tabs[0].options.mode = SearchMode::Literal;
            view.input
                .update(cx, |input, cx| input.emplace("(job #7842)", cx));
            view.start_search(0, cx);
            let _ = window;
        });
        cx.run_until_parked();
        cx.refresh().unwrap();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].results.matches, vec![1]);
            assert!(view.tabs[0].highlight_regex.is_some());
        });

        // Nested capture groups and invalid regexes never panic.
        view.update_in(cx, |view, _window, cx| {
            view.tabs[0].options.mode = SearchMode::Regex;
            view.input
                .update(cx, |input, cx| input.emplace(r"(user=(\w+))", cx));
            view.start_search(0, cx);
        });
        cx.run_until_parked();
        cx.refresh().unwrap();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].results.matches, vec![0]);
            assert!(view.tabs[0].highlight_regex.is_some());
        });

        // Exclude matches pauses highlighting (clears active highlight regex) while preserving user preference.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::ToggleInvert, window, cx);
        });
        cx.run_until_parked();
        cx.refresh().unwrap();
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].options.invert);
            assert!(view.tabs[0].options.highlight_matches);
            assert!(view.tabs[0].highlight_regex.is_none());
        });

        // Turning Exclude matches off restores highlighting automatically.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::ToggleInvert, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].options.invert);
            assert!(view.tabs[0].highlight_regex.is_some());
        });

        // Invalid regex clears highlight regex without panicking.
        view.update_in(cx, |view, _window, cx| {
            view.input.update(cx, |input, cx| input.emplace("[", cx));
            view.start_search(0, cx);
        });
        cx.run_until_parked();
        cx.refresh().unwrap();
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].error.is_some());
            assert!(view.tabs[0].highlight_regex.is_none());
        });
    }

    #[test]
    fn theme_uses_configured_match_highlight_foreground_and_background_colors() {
        let default_config = Config::load(None).unwrap();
        let default_theme = Theme::from_config_with_fonts(&default_config, &[]);
        assert_eq!(default_theme.match_highlight_fg, rgb_to_hsla(rgb(0x0B1016)));
        assert_eq!(default_theme.match_highlight_bg, rgb_to_hsla(rgb(0xE0AF4B)));
        assert_ne!(
            default_theme.match_highlight_bg,
            default_theme.match_highlight_fg
        );
        assert_ne!(default_theme.match_highlight_bg, default_theme.background);
        assert_ne!(default_theme.match_highlight_bg, default_theme.selection);

        let mut custom_config = default_config;
        custom_config.appearance.match_foreground = "#FFFFFF".into();
        custom_config.appearance.match_background = "#2563EB".into();
        let custom_theme = Theme::from_config_with_fonts(&custom_config, &[]);
        assert_eq!(custom_theme.match_highlight_fg, rgb_to_hsla(rgb(0xFFFFFF)));
        assert_eq!(custom_theme.match_highlight_bg, rgb_to_hsla(rgb(0x2563EB)));
        assert_ne!(
            custom_theme.match_highlight_fg,
            default_theme.match_highlight_fg
        );
        assert_ne!(
            custom_theme.match_highlight_bg,
            default_theme.match_highlight_bg
        );
    }

    #[test]
    fn formats_minus_and_shifted_minus_shortcuts_and_preserves_explicit_underscore_bindings() {
        if cfg!(target_os = "macos") {
            assert_eq!(theme::format_shortcut("shift--"), "⇧-");
            assert_eq!(theme::format_shortcut("alt--"), "⌥-");
            assert_eq!(theme::format_shortcut("alt-shift--"), "⌥⇧-");
            assert_eq!(theme::format_shortcut("secondary-alt-h"), "⌘⌥H");
        } else {
            assert_eq!(theme::format_shortcut("shift--"), "Shift+-");
            assert_eq!(theme::format_shortcut("alt--"), "Alt+-");
            assert_eq!(theme::format_shortcut("alt-shift--"), "Alt+Shift+-");
            assert_eq!(theme::format_shortcut("secondary-alt-h"), "Ctrl+Alt+H");
        }
        assert_eq!(theme::format_shortcut("-"), "-");

        // Default config adds an automatic `_` companion alias in Log context for `shift--`.
        let default_config = Config::load(None).unwrap();
        let default_bindings = keybindings(&default_config).unwrap();
        let underscore = Keystroke::parse("_").unwrap();
        assert!(default_bindings.iter().any(|b| {
            b.keystrokes().len() == 1
                && b.keystrokes()[0].inner() == &underscore
                && b.action()
                    .as_any()
                    .downcast_ref::<Execute>()
                    .is_some_and(|ex| ex.command == Command::DecreaseFilterHeight)
        }));

        // Explicit user binding on `_` in Log is not overwritten by the automatic alias.
        let mut custom = default_config;
        custom.keyboard.bindings.push(config::Binding {
            keys: "_".into(),
            context: config::KeyContext::Log,
            action: Command::Reload,
        });
        let custom_bindings = keybindings(&custom).unwrap();
        let matches: Vec<Command> = custom_bindings
            .iter()
            .filter(|b| b.keystrokes().len() == 1 && b.keystrokes()[0].inner() == &underscore)
            .filter_map(|b| {
                b.action()
                    .as_any()
                    .downcast_ref::<Execute>()
                    .map(|e| e.command)
            })
            .collect();
        assert_eq!(matches, vec![Command::Reload]);
    }

    #[gpui::test]
    fn filter_pane_resizes_to_hide_either_pane_with_smooth_motion_and_log_context_shortcuts(
        cx: &mut TestAppContext,
    ) {
        use resize::BASE_RESIZE_STEP;

        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resize.log");
        let content = (0..30)
            .map(|i| {
                if i % 3 == 0 {
                    format!("ERROR line {i}\n")
                } else {
                    format!("INFO line {i}\n")
                }
            })
            .collect::<String>();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let matches: Vec<usize> = (0..30).filter(|i| i % 3 == 0).collect();
        let results = SearchResults {
            matches: matches.clone(),
            visible: matches,
            limited: false,
            widest_row: 0,
        };

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].query = "ERROR".into();
            view.tabs[0].results = results;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        let step_frame = |cx: &mut VisualTestContext, ms: u64| {
            cx.background_executor
                .advance_clock(Duration::from_millis(ms));
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
        };
        let settle = |cx: &mut VisualTestContext| {
            for _ in 0..45 {
                step_frame(cx, 16);
            }
        };

        // Check shortcut tooltip strings (`-` and `⇧-` / `Shift+-`) and initial bounds (`0..max_h`).
        let expected_shrink = if cfg!(target_os = "macos") {
            "⇧-"
        } else {
            "Shift+-"
        };
        let (min_h, max_h) = view.update_in(cx, |view, window, _| {
            assert_eq!(view.results_height, 250.0);
            assert_eq!(
                view.theme
                    .shortcut(Command::IncreaseFilterHeight)
                    .as_deref(),
                Some("-")
            );
            assert_eq!(
                view.theme
                    .shortcut(Command::DecreaseFilterHeight)
                    .as_deref(),
                Some(expected_shrink)
            );
            view.results_height_bounds(window)
        });
        assert_eq!(min_h, 0.0);
        assert!(max_h > 400.0);
        assert!(cx.debug_bounds("source-pane").is_some());
        assert!(cx.debug_bounds("results-body").is_some());
        let file_path_before = cx
            .debug_bounds("file-path-bar")
            .expect("file path bar should render");

        // Dragging all the way to the top hides the main log pane, stops below the file path bar,
        // and hands focus to the filter pane.
        let sash = cx
            .debug_bounds("results-resize-sash")
            .expect("resize sash should render")
            .center();
        let drag_top = point(sash.x, px(0.0));
        cx.simulate_mouse_down(sash, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(drag_top, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(drag_top, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        view.update_in(cx, |view, window, _| {
            assert_eq!(view.results_height, max_h);
            assert!(
                view.results_focus.is_focused(window),
                "hiding main pane should transfer focus to filter pane so Log shortcuts stay active"
            );
        });
        assert!(
            cx.debug_bounds("source-pane").is_none(),
            "main source pane should be hidden at max height"
        );
        let file_path_at_max = cx
            .debug_bounds("file-path-bar")
            .expect("file path bar should remain visible at max filter height");
        let sash_at_max = cx
            .debug_bounds("results-resize-sash")
            .expect("resize sash should remain visible at max filter height");
        assert_eq!(file_path_at_max.origin.y, file_path_before.origin.y);
        assert_eq!(file_path_at_max.size.height, px(FILE_PATH_BAR_HEIGHT));
        assert!(
            sash_at_max.origin.y >= file_path_at_max.bottom(),
            "resize sash ({sash_at_max:?}) should stop at or below file path bar ({file_path_at_max:?})"
        );
        assert!(
            (sash_at_max.origin.y - file_path_at_max.bottom()).abs() <= px(0.5),
            "resize sash ({sash_at_max:?}) should meet the bottom of the file path bar ({file_path_at_max:?})"
        );
        assert!(cx.debug_bounds("results-body").is_some());

        // Pressing `shift--` while main is hidden smoothly restores the main pane.
        let key_shift_minus = Keystroke::parse("shift--").unwrap();
        cx.simulate_keystrokes("shift--");
        cx.simulate_event(KeyUpEvent {
            keystroke: key_shift_minus.clone(),
        });
        step_frame(cx, 16);
        view.read_with(cx, |view, _| {
            assert!(
                view.results_height < max_h && view.results_height > max_h - BASE_RESIZE_STEP,
                "single press should interpolate smoothly rather than snapping immediately"
            );
        });
        settle(cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.results_height, max_h - BASE_RESIZE_STEP);
        });
        assert!(cx.debug_bounds("source-pane").is_some());

        // Dragging all the way to the bottom hides the filter pane and transfers focus to main pane.
        let sash_mid = cx
            .debug_bounds("results-resize-sash")
            .expect("resize sash")
            .center();
        let drag_bottom = point(sash_mid.x, px(2000.0));
        cx.simulate_mouse_down(sash_mid, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(drag_bottom, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(drag_bottom, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        view.update_in(cx, |view, window, _| {
            assert_eq!(view.results_height, 0.0);
            assert!(
                view.focus.is_focused(window),
                "hiding filter pane should transfer focus to main pane so Log shortcuts stay active"
            );
        });
        assert!(
            cx.debug_bounds("results-body").is_none(),
            "filter body should be hidden at 0 height"
        );
        assert!(cx.debug_bounds("source-pane").is_some());

        // When the filter pane is hidden, opening saved patterns flips the dropdown upward.
        view.update_in(cx, |view, window, cx| view.toggle_presets(window, cx));
        cx.run_until_parked();
        let trigger = cx
            .debug_bounds("presets-trigger")
            .expect("presets trigger when hidden");
        let menu = cx
            .debug_bounds("presets-menu")
            .expect("presets menu when hidden");
        assert!(
            menu.bottom() <= trigger.origin.y,
            "presets menu ({menu:?}) should open upward above trigger ({trigger:?}) when filter pane is hidden"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // Double-clicking the split sash smoothly glides back to the configured default (250px).
        let sash_bottom = cx
            .debug_bounds("results-resize-sash")
            .expect("sash when filter hidden")
            .center();
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: sash_bottom,
            modifiers: Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_mouse_up(sash_bottom, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        step_frame(cx, 16);
        view.read_with(cx, |view, _| {
            assert!(
                view.results_height > 0.0 && view.results_height < 250.0,
                "double-click reset should transition smoothly across frames"
            );
        });
        settle(cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.results_height, 250.0);
            assert!(!view.resize_motion.is_animating());
        });

        // Header step buttons step cleanly without stealing focus.
        let grow_btn = cx
            .debug_bounds("results-grow")
            .expect("grow button")
            .center();
        cx.simulate_click(grow_btn, Modifiers::none());
        cx.run_until_parked();
        settle(cx);
        view.update_in(cx, |view, window, _| {
            assert_eq!(view.results_height, 250.0 + BASE_RESIZE_STEP);
            assert!(view.focus.is_focused(window));
        });
        let shrink_btn = cx
            .debug_bounds("results-shrink")
            .expect("shrink button")
            .center();
        cx.simulate_click(shrink_btn, Modifiers::none());
        cx.run_until_parked();
        settle(cx);
        view.read_with(cx, |view, _| assert_eq!(view.results_height, 250.0));

        // In Search and GoTo inputs, `-` and `_` type normally and NEVER resize the filter pane.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("err-code_42");
        cx.run_until_parked();
        settle(cx);
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "err-code_42");
            assert_eq!(view.results_height, 250.0);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // In Log context, `-` grows and native macOS `_` (Shift+-) shrinks cleanly.
        let key_minus = Keystroke::parse("-").unwrap();
        let key_macos_underscore = Keystroke {
            modifiers: Modifiers::none(),
            key: "_".into(),
            key_char: Some("_".into()),
        };
        cx.simulate_keystrokes("-");
        cx.simulate_event(KeyUpEvent {
            keystroke: key_minus.clone(),
        });
        settle(cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.results_height, 250.0 + BASE_RESIZE_STEP);
        });
        cx.simulate_event(KeyDownEvent {
            keystroke: key_macos_underscore.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(KeyUpEvent {
            keystroke: key_macos_underscore,
        });
        settle(cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.results_height, 250.0);
        });

        // Sustained long press of `-` engages continuous smooth velocity-gradient glide.
        cx.simulate_event(KeyDownEvent {
            keystroke: key_minus.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        for _ in 0..15 {
            step_frame(cx, 16);
        }
        view.read_with(cx, |view, _| {
            assert!(
                view.resize_motion.is_long_pressing(),
                "holding `-` past LONG_PRESS_DELAY should engage smooth long-press glide"
            );
        });
        let h_early = view.read_with(cx, |view, _| view.results_height);
        step_frame(cx, 16);
        let early_frame_delta = view.read_with(cx, |view, _| view.results_height - h_early);
        assert!(early_frame_delta > 0.5);

        // Keep holding for ~400ms with OS repeat heartbeats: per-frame glide velocity accelerates.
        for i in 0..24 {
            if i % 3 == 0 {
                cx.simulate_event(KeyDownEvent {
                    keystroke: key_minus.clone(),
                    is_held: true,
                    prefer_character_input: false,
                });
            }
            step_frame(cx, 16);
        }
        let h_late = view.read_with(cx, |view, _| view.results_height);
        step_frame(cx, 16);
        let late_frame_delta = view.read_with(cx, |view, _| view.results_height - h_late);
        assert!(
            late_frame_delta > early_frame_delta * 1.3,
            "long-press velocity gradient should accelerate smoothly (early={early_frame_delta}, late={late_frame_delta})"
        );

        // Releasing the key stops the hold immediately and settles smoothly onto target.
        cx.simulate_event(KeyUpEvent {
            keystroke: key_minus,
        });
        view.read_with(cx, |view, _| {
            assert!(!view.resize_motion.is_long_pressing());
        });
        settle(cx);
        view.read_with(cx, |view, _| {
            assert!(!view.resize_motion.is_animating());
            assert_eq!(view.results_height, view.target_results_height);
        });
    }

    #[gpui::test]
    fn filter_pane_max_height_stops_at_file_path_bar_and_keeps_path_bar_usable(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        std::fs::write(&first, "ERROR one\nINFO two\n").unwrap();
        std::fs::write(&second, "ERROR alpha\nINFO beta\nERROR gamma\n").unwrap();
        let second_canon = second.canonicalize().unwrap();

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![first], window, cx));
        wait_for_tab_loaded(&view, 0, cx);
        view.update(cx, |view, cx| {
            view.input
                .update(cx, |input, cx| input.emplace("ERROR", cx));
            view.start_search(0, cx);
        });
        cx.run_until_parked();

        let step_frame = |cx: &mut VisualTestContext, ms: u64| {
            cx.background_executor
                .advance_clock(Duration::from_millis(ms));
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.run_until_parked();
        };

        let (_, max_h) = view.update_in(cx, |view, window, _| view.results_height_bounds(window));

        // Near max height (10px shy), the file path bar must stay full height and unclipped.
        view.update(cx, |view, cx| {
            view.results_height = max_h - 10.0;
            view.target_results_height = max_h - 10.0;
            cx.notify();
        });
        cx.run_until_parked();
        let path_near_max = cx
            .debug_bounds("file-path-bar")
            .expect("file path bar near max");
        let sash_near_max = cx
            .debug_bounds("results-resize-sash")
            .expect("sash near max");
        let source_near_max = cx
            .debug_bounds("source-pane")
            .expect("source pane should still render 10px shy of max");
        assert_eq!(path_near_max.size.height, px(FILE_PATH_BAR_HEIGHT));
        assert!(source_near_max.origin.y >= path_near_max.bottom());
        assert!(sash_near_max.origin.y >= source_near_max.bottom());

        // Holding `-` glides to max_h and stops right below the file path bar without covering it.
        let key_minus = Keystroke::parse("-").unwrap();
        cx.simulate_event(KeyDownEvent {
            keystroke: key_minus.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        for i in 0..80 {
            if i % 3 == 0 {
                cx.simulate_event(KeyDownEvent {
                    keystroke: key_minus.clone(),
                    is_held: true,
                    prefer_character_input: false,
                });
            }
            step_frame(cx, 16);
        }
        cx.simulate_event(KeyUpEvent {
            keystroke: key_minus,
        });
        for _ in 0..30 {
            step_frame(cx, 16);
        }

        view.update_in(cx, |view, window, _| {
            assert_eq!(view.results_height, max_h);
            assert!(view.results_focus.is_focused(window));
        });
        let path_at_max = cx
            .debug_bounds("file-path-bar")
            .expect("file path bar must stay visible at max height");
        let sash_at_max = cx
            .debug_bounds("results-resize-sash")
            .expect("resize sash at max height");
        assert!(cx.debug_bounds("source-pane").is_none());
        assert_eq!(path_at_max.size.height, px(FILE_PATH_BAR_HEIGHT));
        assert!(
            (sash_at_max.origin.y - path_at_max.bottom()).abs() <= px(0.5),
            "sash ({sash_at_max:?}) should stop directly beneath file path bar ({path_at_max:?})"
        );

        // Focusing and submitting the file path bar at max height keeps the filter pane maximized
        // and returns keyboard focus to the visible filter pane.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(
                view.results_height, max_h,
                "focusing always-visible file path bar should not shrink maximized filter pane"
            );
            assert!(view.path_input_focused(window, cx));
        });
        cx.simulate_input(&second.display().to_string());
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 0, cx);
        view.update_in(cx, |view, window, _| {
            assert_eq!(view.tabs[0].path, second_canon);
            assert_eq!(view.results_height, max_h);
            assert!(
                view.results_focus.is_focused(window),
                "submitting file path while main pane is hidden should focus visible filter pane"
            );
        });

        // Clicking the non-input portion of the file path bar while main is hidden keeps focus
        // on the visible filter pane so Log shortcuts remain active.
        let right_edge = point(path_at_max.right() - px(8.), path_at_max.center().y);
        cx.simulate_mouse_down(right_edge, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(right_edge, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        view.update_in(cx, |view, window, _| {
            assert!(
                view.results_focus.is_focused(window),
                "clicking file path bar chrome while main is hidden should keep filter pane focused"
            );
        });
    }

    #[test]
    fn theme_uses_configured_text_selection_color_and_opacity() {
        let default_config = Config::load(None).unwrap();
        let default_theme = Theme::from_config_with_fonts(&default_config, &[]);
        let expected_default = rgb_to_hsla(rgb(0x38BDF8)).opacity(0.35);
        assert_eq!(default_theme.text_selection, expected_default);
        assert_eq!(
            default_theme.text_selection_soft,
            rgb_to_hsla(rgb(0x38BDF8)).opacity(0.35 * 0.55)
        );

        let mut custom_config = default_config;
        custom_config.appearance.text_selection_color = "#FF5500".into();
        custom_config.appearance.text_selection_opacity = 0.72;
        let custom_theme = Theme::from_config_with_fonts(&custom_config, &[]);
        assert_eq!(
            custom_theme.text_selection,
            rgb_to_hsla(rgb(0xFF5500)).opacity(0.72)
        );
        assert_eq!(
            custom_theme.text_selection_soft,
            rgb_to_hsla(rgb(0xFF5500)).opacity(0.72 * 0.55)
        );
    }

    #[gpui::test]
    fn row_text_selection_works_in_main_and_filter_panes_with_drag_word_triple_click_and_shift(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.max_display_chars = 48;
        config.appearance.text_selection_color = "#FF5500".into();
        config.appearance.text_selection_opacity = 0.6;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("text-select.log");
        let lines = [
            "INFO service started request_id=req_1001",
            "ERROR database timeout user=alice_42 retry=3",
            "INFO skipping unrelated noise",
            "ERROR gateway failed status=503 host=edge",
            "ERROR truncated payload 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
        ];
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let results = SearchResults {
            matches: vec![1, 3, 4],
            visible: vec![1, 3, 4],
            limited: false,
            widest_row: 2,
        };
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].query = "ERROR".into();
            view.tabs[0].sync_highlight_regex();
            view.tabs[0].results = results;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        let copy_key = if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-c"
        };

        // 1. Horizontal drag on main-text-0 selects a substring (not the whole line) and copies it.
        let main0 = cx
            .debug_bounds("main-text-0")
            .expect("main text 0 should render");
        let drag_start = point(main0.origin.x + px(1.0), main0.center().y);
        let drag_end = point(main0.origin.x + main0.size.width * 0.3, main0.center().y);
        cx.simulate_mouse_down(drag_start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(drag_end, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(drag_end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].has_text_selection(Pane::Main));
            assert!(view.text_drag.is_none());
            let sel = view.tabs[0].main_text_selection.unwrap();
            assert_eq!(sel.row_range(), 0..=0);
            assert!(sel.ordered().0.offset < sel.ordered().1.offset);
        });
        cx.simulate_keystrokes(copy_key);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain copied substring");
        assert!(
            !copied.is_empty() && copied.len() < lines[0].len() && lines[0].starts_with(&copied),
            "expected prefix substring of line 0, got {copied:?}"
        );

        // 2. Shift+click on main-text-1 extends the active text selection across rows 0..=1.
        let main1 = cx
            .debug_bounds("main-text-1")
            .expect("main text 1 should render");
        let shift_target = point(main1.origin.x + main1.size.width * 0.25, main1.center().y);
        cx.simulate_click(shift_target, Modifiers::shift());
        cx.run_until_parked();
        cx.simulate_keystrokes(copy_key);
        let multi_copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain multi-row substring");
        assert!(
            multi_copied.contains('\n'),
            "multi-row text selection should include newline: {multi_copied:?}"
        );
        let parts: Vec<&str> = multi_copied.split('\n').collect();
        assert_eq!(parts.len(), 2);
        assert!(lines[0].ends_with(parts[0]));
        assert!(lines[1].starts_with(parts[1]));

        // 3. Escape clears only the text selection.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].has_text_selection(Pane::Main));
        });

        // 4. Double-click on main-text-1 selects the word under the pointer.
        let word_pt = point(main1.origin.x + px(4.0), main1.center().y);
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: word_pt,
            modifiers: Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_mouse_up(word_pt, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes(copy_key);
        let word_copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain double-clicked word");
        assert_eq!(word_copied, "ERROR");

        // 5. Triple-click on truncated main-text-4 selects the displayed line text excluding the truncation marker.
        let main4 = cx
            .debug_bounds("main-text-4")
            .expect("main text 4 should render");
        let triple_pt = point(main4.origin.x + px(8.0), main4.center().y);
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: triple_pt,
            modifiers: Modifiers::none(),
            click_count: 3,
            first_mouse: false,
        });
        cx.simulate_mouse_up(triple_pt, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes(copy_key);
        let triple_copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain triple-clicked line");
        assert_eq!(triple_copied.chars().count(), 48);
        assert!(!triple_copied.contains("[truncated]"));

        // 6. Filter (results) pane: horizontal drag, multi-row drag, double-click, and Escape all work.
        let res0 = cx
            .debug_bounds("result-text-0")
            .expect("result text 0 should render");
        let res1 = cx
            .debug_bounds("result-text-1")
            .expect("result text 1 should render");
        let r_start = point(res0.origin.x + px(1.0), res0.center().y);
        let r_end = point(res0.origin.x + res0.size.width * 0.35, res0.center().y);
        cx.simulate_mouse_down(r_start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(r_end, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(r_end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        view.update_in(cx, |view, window, _| {
            assert!(view.results_focus.is_focused(window));
            assert!(view.tabs[0].has_text_selection(Pane::Results));
        });
        cx.simulate_keystrokes(copy_key);
        let res_copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain copied filter-pane substring");
        assert!(
            !res_copied.is_empty()
                && res_copied.len() < lines[1].len()
                && lines[1].starts_with(&res_copied),
            "expected prefix substring of filtered line 1, got {res_copied:?}"
        );

        // Multi-row drag across filtered rows 0 -> 1 skips non-matching source line 2.
        let r_multi_end = point(res1.origin.x + res1.size.width * 0.25, res1.center().y);
        cx.simulate_mouse_down(r_start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(r_multi_end, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(r_multi_end, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes(copy_key);
        let res_multi = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain multi-row filter selection");
        let res_parts: Vec<&str> = res_multi.split('\n').collect();
        assert_eq!(res_parts.len(), 2);
        assert!(lines[1].ends_with(res_parts[0]));
        assert!(lines[3].starts_with(res_parts[1]));
        assert!(!res_multi.contains("skipping unrelated noise"));

        // Escape clears filter-pane text selection.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].has_text_selection(Pane::Results));
        });
    }

    #[gpui::test]
    fn text_selected_context_overrides_log_and_selection_only_when_focused_pane_has_text_selection(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        // Bind `x` to bookmark in Log, swap_selection_anchor in Selection, and copy_selected_text in TextSelected.
        config.keyboard.bindings.push(config::Binding {
            keys: "x".into(),
            context: config::KeyContext::Log,
            action: Command::ToggleBookmark,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "x".into(),
            context: config::KeyContext::Selection,
            action: Command::SwapSelectionAnchor,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "x".into(),
            context: config::KeyContext::TextSelected,
            action: Command::CopySelectedText,
        });
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("text-ctx.log");
        std::fs::write(
            &path,
            "ERROR alpha token\nINFO beta token\nERROR gamma token\n",
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let results = SearchResults {
            matches: vec![0, 2],
            visible: vec![0, 2],
            limited: false,
            widest_row: 0,
        };
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].query = "ERROR".into();
            view.tabs[0].results = results;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        // Without text selection, `x` runs Log (ToggleBookmark on line 0).
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
        });

        // With BOTH multi-line selection AND text selection in main pane, TextSelected takes precedence over Selection and Log.
        view.update(cx, |view, cx| {
            view.tabs[0].main_selection.extend_to(2);
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 6),
                TextPosition::new(0, 11),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("alpha")
        );
        view.read_with(cx, |view, _| {
            // Neither Log (ToggleBookmark) nor Selection (SwapSelectionAnchor) ran.
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
            assert_eq!(view.tabs[0].main_selection.anchor, 0);
            assert_eq!(view.tabs[0].main_selection.cursor, 2);
        });

        // Focusing Search while main pane has text selected must NOT activate TextSelected.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("x");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "x");
        });

        // Focusing Filter pane (no text selection there) while main pane still has text selected must NOT activate TextSelected.
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Results, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            // Filter row 0 maps to source line 0, so Log's ToggleBookmark unbookmarked line 0.
            assert!(view.tabs[0].bookmarks.is_empty());
        });

        // Selecting text in Filter pane activates TextSelected for Filter pane.
        view.update(cx, |view, cx| {
            view.tabs[0].result_text_selection = Some(TextSelection::new(
                TextPosition::new(1, 6),
                TextPosition::new(1, 11),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("gamma")
        );
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].bookmarks.is_empty());
        });

        // Pressing Escape clears filter text selection, returning `x` to Log context.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].has_text_selection(Pane::Results));
        });
        cx.simulate_keystrokes("x");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
        });
    }

    #[gpui::test]
    fn f_in_text_selected_inserts_into_search_focuses_and_places_caret_at_end(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("f-shortcut.log");
        std::fs::write(&path, "INFO Hello World\nERROR Gamma Delta\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let results = SearchResults {
            matches: vec![1],
            visible: vec![1],
            limited: false,
            widest_row: 0,
        };
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].results = results;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            view.input.update(cx, |input, cx| input.emplace("old", cx));
            cx.notify();
        });
        cx.run_until_parked();

        // Without text selection, `f` still toggles follow in Log context.
        cx.simulate_keystrokes("f");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.tabs[0].follow);
            assert_eq!(view.input.read(cx).as_str(), "old");
        });

        // Select "Hello" in main pane and press `f`: inserts into search box, focuses it, places caret at end.
        view.update(cx, |view, cx| {
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 5),
                TextPosition::new(0, 10),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("f");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(
                view.tabs[0].follow,
                "TextSelected `f` must not toggle follow"
            );
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "Hello");
            let end = "Hello".encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
        });

        // Typing immediately appends at the end (proving cursor is at end with no selection,
        // and `f` in Search context types literal `f`).
        cx.simulate_input("f");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "Hellof");
        });

        // Works from Filter/Results pane too (select "Delta").
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Results, window, cx);
            view.tabs[0].result_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 12),
                TextPosition::new(0, 17),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("f");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "Delta");
            let end = "Delta".encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
        });
    }

    #[gpui::test]
    fn a_in_text_selected_and_regex_mode_appends_or_copies_and_places_caret_at_end(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a-shortcut.log");
        std::fs::write(&path, "INFO Hello World\nERROR Gamma Delta\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let results = SearchResults {
            matches: vec![1],
            visible: vec![1],
            limited: false,
            widest_row: 0,
        };
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].results = results;
            view.tabs[0].options.mode = SearchMode::Literal;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 11),
                TextPosition::new(0, 16),
            ));
            view.input
                .update(cx, |input, cx| input.emplace("Hello", cx));
            cx.notify();
        });
        cx.run_until_parked();

        // In Literal mode, `a` does nothing even when text is selected.
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "Hello");
        });
        view.update_in(cx, |view, window, cx| {
            assert!(!view.input.read(cx).focus_handle(cx).is_focused(window));
        });

        // In RegexMode with NO text selected, `a` does nothing.
        view.update(cx, |view, cx| {
            view.tabs[0].options.mode = SearchMode::Regex;
            view.tabs[0].main_text_selection = None;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "Hello");
            assert!(!view.input.read(cx).focus_handle(cx).is_focused(window));
        });

        // In RegexMode with "Hello" in search bar and "World" selected: becomes "Hello|World^".
        view.update(cx, |view, cx| {
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 11),
                TextPosition::new(0, 16),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "Hello|World");
            let end = "Hello|World".encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
        });

        // Typing immediately appends at the end (proving cursor is at end with no selection,
        // and `a` in Search context types literal `a`).
        cx.simulate_input("a");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "Hello|Worlda");
        });

        // When search bar is empty in RegexMode, `a` copies the selected text without leading `|`.
        view.update_in(cx, |view, window, cx| {
            view.input.update(cx, |input, cx| input.emplace("", cx));
            view.focus_pane(Pane::Results, window, cx);
            view.tabs[0].result_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 6),
                TextPosition::new(0, 11),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "Gamma");
            let end = "Gamma".encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
        });

        // Trailing `|` does not produce `||`.
        view.update_in(cx, |view, window, cx| {
            view.input
                .update(cx, |input, cx| input.emplace("Gamma|", cx));
            view.focus_pane(Pane::Results, window, cx);
            view.tabs[0].result_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 12),
                TextPosition::new(0, 17),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "Gamma|Delta");
        });
    }

    #[gpui::test]
    fn text_selected_regex_mode_context_precedence_is_independent_of_binding_order(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        // Intentionally register `TextSelected && RegexMode` BEFORE `TextSelected` for the same key.
        config.keyboard.bindings.push(config::Binding {
            keys: "z".into(),
            context: config::KeyContext::TextSelectedRegexMode,
            action: Command::AppendSelectedTextToSearch,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "z".into(),
            context: config::KeyContext::TextSelected,
            action: Command::SearchSelectedText,
        });
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("precedence.log");
        std::fs::write(&path, "INFO World\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].options.mode = SearchMode::Regex;
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 5),
                TextPosition::new(0, 10),
            ));
            view.input
                .update(cx, |input, cx| input.emplace("Hello", cx));
            cx.notify();
        });
        cx.run_until_parked();

        // In RegexMode, `TextSelected && RegexMode` wins over `TextSelected`.
        cx.simulate_keystrokes("z");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "Hello|World");
        });

        // In Literal mode, `TextSelected` wins.
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Main, window, cx);
            view.tabs[0].options.mode = SearchMode::Literal;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("z");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "World");
        });
    }

    #[gpui::test]
    fn f_and_a_escape_regex_metacharacters_in_regex_mode_only(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("regex-escape.log");
        let line = "[ERR] req(1.2)+$a|b";
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].options.mode = SearchMode::Literal;
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 0),
                TextPosition::new(0, line.len()),
            ));
            cx.notify();
        });
        cx.run_until_parked();

        // In Literal mode, `f` inserts the raw selected text without escaping and places the caret at the end.
        cx.simulate_keystrokes("f");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), line);
            let end = line.encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
        });

        // In Regex mode, `f` escapes regex metacharacters and places the caret at the end.
        let escaped = regex::escape(line);
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Main, window, cx);
            view.tabs[0].options.mode = SearchMode::Regex;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("f");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), escaped);
            let end = escaped.encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
            assert!(Regex::new(&escaped).unwrap().is_match(line));
        });

        // In Regex mode, `a` escapes when copying into an empty search box.
        let part_a = "[ERR]";
        let part_b = "req(1.2)+$a|b";
        let escaped_a = regex::escape(part_a);
        let escaped_b = regex::escape(part_b);
        view.update_in(cx, |view, window, cx| {
            view.input.update(cx, |input, cx| input.emplace("", cx));
            view.focus_pane(Pane::Main, window, cx);
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 0),
                TextPosition::new(0, part_a.len()),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), escaped_a);
            let end = escaped_a.encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
        });

        // In Regex mode, `a` escapes when appending after `|` and places the caret at the end.
        let expected_combined = format!("{escaped_a}|{escaped_b}");
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Main, window, cx);
            view.tabs[0].main_text_selection = Some(TextSelection::new(
                TextPosition::new(0, 6),
                TextPosition::new(0, line.len()),
            ));
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), expected_combined);
            let end = expected_combined.encode_utf16().count();
            let sel = view
                .input
                .update(cx, |input, cx| input.selected_text_range(false, window, cx))
                .expect("selection range");
            assert_eq!(sel.range, end..end);
            let re = Regex::new(&expected_combined).unwrap();
            assert!(re.is_match(part_a));
            assert!(re.is_match(part_b));
        });
    }

    #[gpui::test]
    fn middle_clicking_a_tab_closes_only_that_tab(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        let third = directory.path().join("third.log");
        let fourth = directory.path().join("fourth.log");
        std::fs::write(&first, "one\n").unwrap();
        std::fs::write(&second, "two\n").unwrap();
        std::fs::write(&third, "three\n").unwrap();
        std::fs::write(&fourth, "four\n").unwrap();
        let first_canon = first.canonicalize().unwrap_or_else(|_| first.clone());
        let second_canon = second.canonicalize().unwrap_or_else(|_| second.clone());
        let third_canon = third.canonicalize().unwrap_or_else(|_| third.clone());
        let fourth_canon = fourth.canonicalize().unwrap_or_else(|_| fourth.clone());

        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(config, vec![first, second, third, fourth], window, cx)
        });
        view.update(cx, |view, cx| {
            view.switch(1, cx);
            view.input
                .update(cx, |input, cx| input.emplace("active-query", cx));
        });
        cx.run_until_parked();

        // Right-clicking a tab does not close it.
        let tab0_bounds = cx.debug_bounds("tab-0").expect("tab 0 bounds");
        let tab0_body = point(tab0_bounds.left() + px(12.), tab0_bounds.center().y);
        cx.simulate_mouse_down(tab0_body, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(tab0_body, MouseButton::Right, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 4);
            assert_eq!(view.active, 1);
        });

        // Middle-clicking empty space in the tab strip does not close any tab.
        let strip_bounds = cx.debug_bounds("tab-strip").expect("tab strip bounds");
        let tab3_bounds = cx.debug_bounds("tab-3").expect("tab 3 bounds");
        assert!(
            strip_bounds.right() > tab3_bounds.right() + px(8.),
            "expected empty space in tab strip to test non-tab middle clicks"
        );
        let empty_strip_pt = point(strip_bounds.right() - px(4.), strip_bounds.center().y);
        cx.simulate_mouse_down(empty_strip_pt, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_up(empty_strip_pt, MouseButton::Middle, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 4);
            assert_eq!(view.active, 1);
        });

        // Pressing middle mouse on a tab and releasing off the tab cancels closing.
        let tab2_bounds = cx.debug_bounds("tab-2").expect("tab 2 bounds");
        let tab2_body = point(tab2_bounds.left() + px(12.), tab2_bounds.center().y);
        cx.simulate_mouse_down(tab0_body, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_move(tab2_body, Some(MouseButton::Middle), Modifiers::none());
        cx.simulate_mouse_up(tab2_body, MouseButton::Middle, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 4);
            assert_eq!(view.active, 1);
        });

        // Pressing middle mouse outside tabs and releasing over a tab does not close it.
        cx.simulate_mouse_down(empty_strip_pt, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_move(tab0_body, Some(MouseButton::Middle), Modifiers::none());
        cx.simulate_mouse_up(tab0_body, MouseButton::Middle, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 4);
            assert_eq!(view.active, 1);
        });

        // Middle-clicking an inactive tab closes it without switching away or losing active query.
        cx.simulate_mouse_down(tab0_body, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_up(tab0_body, MouseButton::Middle, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let paths: Vec<_> = view.tabs.iter().map(|tab| tab.path.clone()).collect();
            assert_eq!(
                paths,
                vec![
                    second_canon.clone(),
                    third_canon.clone(),
                    fourth_canon.clone()
                ]
            );
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "active-query");
            assert_ne!(view.tabs[0].path, first_canon);
        });

        // Middle-clicking on a tab's inline close button closes that tab once.
        let tab1_close = cx
            .debug_bounds("tab-close-1")
            .expect("tab 1 close button bounds")
            .center();
        cx.simulate_mouse_down(tab1_close, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_up(tab1_close, MouseButton::Middle, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let paths: Vec<_> = view.tabs.iter().map(|tab| tab.path.clone()).collect();
            assert_eq!(paths, vec![second_canon.clone(), fourth_canon.clone()]);
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "active-query");
        });

        // Middle-clicking the active tab closes it and activates the remaining tab.
        let active_tab_bounds = cx.debug_bounds("tab-0").expect("active tab bounds");
        let active_tab_body = point(
            active_tab_bounds.left() + px(12.),
            active_tab_bounds.center().y,
        );
        cx.simulate_mouse_down(active_tab_body, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_up(active_tab_body, MouseButton::Middle, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let paths: Vec<_> = view.tabs.iter().map(|tab| tab.path.clone()).collect();
            assert_eq!(paths, vec![fourth_canon.clone()]);
            assert_eq!(view.active, 0);
        });

        // Middle-clicking the last remaining tab empties the tab list cleanly.
        let last_tab_bounds = cx.debug_bounds("tab-0").expect("last tab bounds");
        let last_tab_body = point(last_tab_bounds.left() + px(12.), last_tab_bounds.center().y);
        cx.simulate_mouse_down(last_tab_body, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_up(last_tab_body, MouseButton::Middle, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.tabs.is_empty());
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "");
            assert_eq!(view.path_input.read(cx).as_str(), "");
        });
    }

    fn wait_for_tab_loaded(view: &Entity<Workspace>, tab_index: usize, cx: &mut VisualTestContext) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            view.update(cx, |view, cx| view.tick(cx));
            cx.run_until_parked();
            let loaded = view.read_with(cx, |view, _| {
                view.tabs
                    .get(tab_index)
                    .is_some_and(|tab| !tab.loading && tab.log.is_some())
            });
            if loaded {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for tab {tab_index} to finish loading"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[gpui::test]
    fn file_path_bar_enter_replaces_active_log_and_cmd_enter_opens_new_tab(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        let third = directory.path().join("third.log");
        std::fs::write(&first, "alpha line\nmatch one\n").unwrap();
        std::fs::write(&second, "beta line\nmatch two\nthird line\n").unwrap();
        std::fs::write(&third, "gamma line\n").unwrap();
        let first_canon = first.canonicalize().unwrap();
        let second_canon = second.canonicalize().unwrap();
        let third_canon = third.canonicalize().unwrap();

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![first], window, cx));
        wait_for_tab_loaded(&view, 0, cx);

        // Initial path bar shows canonical path, stays single-line, and does not open completions.
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.path_input.read(cx).as_str(),
                first_canon.display().to_string()
            );
            assert!(!view.show_path_completions);
        });
        let input_bounds = cx
            .debug_bounds("file-path-input")
            .expect("file path input should render");
        assert!(
            input_bounds.size.height <= px(28.),
            "file path input should stay single-line, got {:?}",
            input_bounds.size.height
        );

        // Set a search query and bookmark on tab 0 before replacing it.
        view.update(cx, |view, cx| {
            view.input
                .update(cx, |input, cx| input.emplace("match", cx));
            view.tabs[0].bookmarks.insert(0);
            view.start_search(0, cx);
        });
        cx.run_until_parked();

        // Clicking the file path input focuses it without popping open completions immediately.
        cx.simulate_click(input_bounds.center(), Modifiers::none());
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.path_input_focused(window, cx));
            assert!(!view.show_path_completions);
        });

        // Typing log-shortcut letters (`m`, `f`) inside the file path input must not trigger Log actions.
        let select_all = if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        };
        let new_tab_enter = if cfg!(target_os = "macos") {
            "cmd-enter"
        } else {
            "ctrl-enter"
        };
        cx.simulate_keystrokes(select_all);
        cx.simulate_input("mf");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.path_input.read(cx).as_str(), "mf");
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
            assert!(!view.tabs[0].follow);
        });

        // Entering second.log and pressing Enter replaces tab 0 in place, preserving query and clearing bookmarks.
        cx.simulate_keystrokes(select_all);
        cx.simulate_input(&second.display().to_string());
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 0, cx);

        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.tabs.len(), 1);
            assert_eq!(view.active, 0);
            assert_eq!(view.tabs[0].path, second_canon);
            assert_eq!(
                view.path_input.read(cx).as_str(),
                second_canon.display().to_string()
            );
            assert!(view.tabs[0].bookmarks.is_empty());
            assert_eq!(view.tabs[0].query, "match");
            assert_eq!(view.tabs[0].results.matches, vec![1]);
            assert_eq!(view.tabs[0].log.as_ref().unwrap().len(), 3);
            assert_eq!(view.recent.first(), Some(&second_canon));
            assert!(view.focus.is_focused(window));
            assert!(!view.show_path_completions);
        });

        // Focusing via FocusFilePath, entering third.log, and pressing Cmd+Enter opens a new tab.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&third.display().to_string());
        cx.run_until_parked();
        cx.simulate_keystrokes(new_tab_enter);
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 1, cx);

        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.active, 1);
            assert_eq!(view.tabs[0].path, second_canon);
            assert_eq!(view.tabs[1].path, third_canon);
            assert_eq!(
                view.path_input.read(cx).as_str(),
                third_canon.display().to_string()
            );
            assert!(view.focus.is_focused(window));
            assert!(!view.show_path_completions);
        });

        // Switching back to tab 0 syncs the file path input without opening completions.
        view.update(cx, |view, cx| view.switch(0, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.path_input.read(cx).as_str(),
                second_canon.display().to_string()
            );
            assert!(!view.show_path_completions);
        });
    }

    #[gpui::test]
    fn file_path_bar_completes_filesystem_names_with_tab_arrows_and_mouse(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let archive = root.join("archive");
        std::fs::create_dir(&archive).unwrap();
        let nested = archive.join("nested.log");
        let app1 = root.join("app-2026-01.log");
        let app2 = root.join("app-2026-02.log");
        let base = root.join("base.log");
        let base_rot = root.join("base.log.1");
        let unique = root.join("unique.log");
        let hidden = root.join(".secret.log");
        for file in [&nested, &app1, &app2, &base, &base_rot, &unique, &hidden] {
            std::fs::write(file, "line\n").unwrap();
        }
        let dir_prefix = ensure_trailing_separator(&root.display().to_string());
        let new_tab_enter = if cfg!(target_os = "macos") {
            "cmd-enter"
        } else {
            "ctrl-enter"
        };

        let (view, cx) = cx
            .add_window_view(|window, cx| Workspace::new(config, vec![unique.clone()], window, cx));
        wait_for_tab_loaded(&view, 0, cx);

        // Typing a directory prefix opens the completion dropdown with matching files.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&format!("{dir_prefix}ap"));
        cx.run_until_parked();
        assert!(cx.debug_bounds("path-completions-menu").is_some());
        view.read_with(cx, |view, _| {
            let labels: Vec<&str> = view
                .path_completions
                .items
                .iter()
                .map(|i| i.label.as_str())
                .collect();
            assert_eq!(labels, vec!["app-2026-01.log", "app-2026-02.log"]);
            assert_eq!(view.path_completion_index, None);
        });

        // Tab extends to the longest common prefix while keeping the dropdown open.
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.path_input.read(cx).as_str(),
                format!("{dir_prefix}app-2026-0")
            );
            assert!(view.show_path_completions);
            assert_eq!(view.path_completion_index, None);
        });

        // Down/Down selects the second match and Cmd+Enter opens it in a new tab.
        cx.simulate_keystrokes("down down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.path_completion_index, Some(1));
        });
        cx.simulate_keystrokes(new_tab_enter);
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 1, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.active, 1);
            assert_eq!(view.tabs[0].path, unique);
            assert_eq!(view.tabs[1].path, app2);
        });

        // Tab on a unique directory completes with trailing separator and immediately lists its children.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&format!("{dir_prefix}ar"));
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let expected_dir = format!("{dir_prefix}archive{}", std::path::MAIN_SEPARATOR);
            assert_eq!(view.path_input.read(cx).as_str(), expected_dir);
            assert!(view.show_path_completions);
            assert_eq!(view.path_completions.items.len(), 1);
            assert_eq!(view.path_completions.items[0].label, "nested.log");
        });

        // Tab on the unique child file completes its path and closes the dropdown; Enter replaces tab 1.
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.path_input.read(cx).as_str(),
                nested.display().to_string()
            );
            assert!(!view.show_path_completions);
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 1, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.active, 1);
            assert_eq!(view.tabs[1].path, nested);
        });

        // Typing exact file `base.log` when `base.log.1` also exists still submits `base.log` on Enter.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&base.display().to_string());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_path_completions);
            assert_eq!(view.path_completion_index, None);
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 1, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.tabs[1].path, base);
        });

        // Mouse click on a completion item replaces active tab; secondary-click opens in a new tab.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&format!("{dir_prefix}app-2026-0"));
        cx.run_until_parked();
        let item0 = cx
            .debug_bounds("path-completion-0")
            .expect("first completion item")
            .center();
        cx.simulate_click(item0, Modifiers::none());
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 1, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.tabs[1].path, app1);
        });

        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&format!("{dir_prefix}app-2026-0"));
        cx.run_until_parked();
        let item1 = cx
            .debug_bounds("path-completion-1")
            .expect("second completion item")
            .center();
        cx.simulate_click(item1, Modifiers::secondary_key());
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 2, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 3);
            assert_eq!(view.active, 2);
            assert_eq!(view.tabs[1].path, app1);
            assert_eq!(view.tabs[2].path, app2);
        });

        // Typing a leading dot reveals dotfiles.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&format!("{dir_prefix}."));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.path_completions.items.len(), 1);
            assert_eq!(view.path_completions.items[0].label, ".secret.log");
        });
    }

    #[gpui::test]
    fn file_path_bar_escape_blur_and_invalid_or_directory_paths_behave_safely(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let valid = root.join("valid.log");
        let subdir = root.join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(&valid, "ok\n").unwrap();
        std::fs::write(subdir.join("child.log"), "child\n").unwrap();
        let missing = root.join("does-not-exist.log");
        let new_tab_enter = if cfg!(target_os = "macos") {
            "cmd-enter"
        } else {
            "ctrl-enter"
        };

        let (view, cx) = cx
            .add_window_view(|window, cx| Workspace::new(config, vec![valid.clone()], window, cx));
        wait_for_tab_loaded(&view, 0, cx);

        // Nonexistent path on Enter and Cmd+Enter keeps focus in the input, sets error status,
        // and leaves the active tab untouched.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&missing.display().to_string());
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.path_input_focused(window, cx));
            assert_eq!(view.tabs.len(), 1);
            assert_eq!(view.tabs[0].path, valid);
            assert!(matches!(
                view.status.as_ref().map(|s| s.tone),
                Some(StatusTone::Error)
            ));
        });

        cx.simulate_keystrokes(new_tab_enter);
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.path_input_focused(window, cx));
            assert_eq!(view.tabs.len(), 1);
            assert_eq!(view.tabs[0].path, valid);
        });

        // Pressing Enter on a directory without trailing separator descends into it and opens completions.
        let select_all = if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        };
        cx.simulate_keystrokes(select_all);
        cx.simulate_input(&subdir.display().to_string());
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.path_input_focused(window, cx));
            assert_eq!(
                view.path_input.read(cx).as_str(),
                ensure_trailing_separator(&subdir.display().to_string())
            );
            assert!(view.show_path_completions);
            assert_eq!(view.path_completions.items.len(), 1);
            assert_eq!(view.path_completions.items[0].label, "child.log");
        });

        // First Escape dismisses the completion dropdown while staying in the path input.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.path_input_focused(window, cx));
            assert!(!view.show_path_completions);
        });

        // Second Escape reverts the text to the active tab's path and returns focus to the log pane.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.focus.is_focused(window));
            assert_eq!(
                view.path_input.read(cx).as_str(),
                valid.display().to_string()
            );
        });

        // Blurring the path bar after a partial edit closes completions and reverts the path.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&ensure_trailing_separator(&root.display().to_string()));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.show_path_completions));
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Main, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.show_path_completions);
            assert_eq!(
                view.path_input.read(cx).as_str(),
                valid.display().to_string()
            );
        });

        // Focusing search while editing the path bar also closes completions and reverts the path.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&ensure_trailing_separator(&root.display().to_string()));
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.show_path_completions));
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.show_path_completions);
            assert_eq!(
                view.path_input.read(cx).as_str(),
                valid.display().to_string()
            );
        });
    }

    #[gpui::test]
    fn previous_tab_cycles_backward_with_wrap_around_and_handles_single_or_empty_tabs(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        let third = directory.path().join("third.log");
        std::fs::write(&first, "one\n").unwrap();
        std::fs::write(&second, "two\n").unwrap();
        std::fs::write(&third, "three\n").unwrap();
        let first_canon = first.canonicalize().unwrap_or_else(|_| first.clone());
        let second_canon = second.canonicalize().unwrap_or_else(|_| second.clone());
        let third_canon = third.canonicalize().unwrap_or_else(|_| third.clone());

        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(config, vec![first, second, third], window, cx)
        });
        cx.run_until_parked();

        // Start on tab 0 with distinct queries across all three tabs.
        view.update(cx, |view, cx| {
            view.switch(0, cx);
            view.tabs[1].query = "q1".into();
            view.tabs[2].query = "q2".into();
            view.input.update(cx, |input, cx| input.emplace("q0", cx));
        });
        cx.run_until_parked();

        // Default shortcut label is populated for PreviousTab.
        view.read_with(cx, |view, _| {
            let expected = if cfg!(target_os = "macos") {
                "⌃⇧⇥"
            } else {
                "Ctrl+Shift+⇥"
            };
            assert_eq!(
                view.theme.shortcut(Command::PreviousTab).as_deref(),
                Some(expected)
            );
        });

        // Ctrl+Shift+Tab wraps from tab 0 to the last tab (2), saving tab 0's query and syncing path.
        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.active, 2);
            assert_eq!(view.tabs[0].query, "q0");
            assert_eq!(view.input.read(cx).as_str(), "q2");
            assert_eq!(
                view.path_input.read(cx).as_str(),
                third_canon.display().to_string()
            );
        });

        // Pressing Ctrl+Shift+Tab again moves to tab 1, then back to tab 0.
        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.active, 1);
            assert_eq!(view.input.read(cx).as_str(), "q1");
            assert_eq!(
                view.path_input.read(cx).as_str(),
                second_canon.display().to_string()
            );
        });

        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "q0");
            assert_eq!(
                view.path_input.read(cx).as_str(),
                first_canon.display().to_string()
            );
        });

        // Works while search input has focus (Workspace context).
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.active, 2);
            assert_eq!(view.input.read(cx).as_str(), "q2");
        });

        // Ctrl+Tab cycles forward back to tab 0.
        cx.simulate_keystrokes("ctrl-tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "q0");
        });

        // With a single tab, PreviousTab keeps index 0 and preserves its query.
        view.update(cx, |view, cx| {
            view.close_tab_at(2, cx);
            view.close_tab_at(1, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs.len(), 1);
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "q0");
        });

        // With no tabs open, PreviousTab is a safe no-op.
        view.update(cx, |view, cx| {
            view.close_tab_at(0, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.tabs.is_empty());
            assert_eq!(view.active, 0);
            assert_eq!(view.input.read(cx).as_str(), "");
        });
    }

    #[gpui::test]
    fn double_clicking_top_ribbon_maximizes_window_without_triggering_on_controls(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        std::fs::write(&first, "alpha\n").unwrap();
        std::fs::write(&second, "beta\n").unwrap();
        let log_first = Arc::new(LogFile::open(&first, &AtomicBool::new(false)).unwrap());
        let log_second = Arc::new(LogFile::open(&second, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, Vec::new(), window, cx));
        cx.run_until_parked();

        let simulate_counted_click = |cx: &mut VisualTestContext,
                                      position: Point<Pixels>,
                                      button: MouseButton,
                                      count: usize| {
            cx.simulate_event(MouseDownEvent {
                button,
                position,
                modifiers: Modifiers::none(),
                click_count: count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button,
                position,
                modifiers: Modifiers::none(),
                click_count: count,
            });
            cx.run_until_parked();
        };

        let simulate_double_click = |cx: &mut VisualTestContext, position: Point<Pixels>| {
            simulate_counted_click(cx, position, MouseButton::Left, 1);
            simulate_counted_click(cx, position, MouseButton::Left, 2);
        };

        // Empty workspace (welcome screen): single click on the top ribbon does not maximize.
        let empty_strip = cx
            .debug_bounds("tab-strip")
            .expect("empty tab strip bounds")
            .center();
        simulate_counted_click(cx, empty_strip, MouseButton::Left, 1);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 0));

        // Double-clicking empty space on the top ribbon toggles maximize.
        simulate_double_click(cx, empty_strip);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 1));

        // Double-clicking on the brand title area also toggles maximize.
        let brand_pt = cx
            .debug_bounds("header-brand")
            .expect("header brand bounds")
            .center();
        simulate_double_click(cx, brand_pt);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 2));

        // Triple-clicking only triggers maximize once on the second click.
        simulate_counted_click(cx, empty_strip, MouseButton::Left, 1);
        simulate_counted_click(cx, empty_strip, MouseButton::Left, 2);
        simulate_counted_click(cx, empty_strip, MouseButton::Left, 3);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 3));

        // Right-clicking and middle-clicking twice on the top ribbon do not maximize.
        simulate_counted_click(cx, empty_strip, MouseButton::Right, 1);
        simulate_counted_click(cx, empty_strip, MouseButton::Right, 2);
        simulate_counted_click(cx, empty_strip, MouseButton::Middle, 1);
        simulate_counted_click(cx, empty_strip, MouseButton::Middle, 2);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 3));

        // Double-clicking the library toolbar button toggles the drawer without maximizing.
        let library_btn = cx
            .debug_bounds("header-library")
            .expect("library button bounds")
            .center();
        simulate_double_click(cx, library_btn);
        view.read_with(cx, |view, _| {
            assert!(!view.show_files);
            assert_eq!(view.header_zoom_count, 3);
        });

        // Open two tabs and test empty strip, tab, follow button, and tab close interactions.
        view.update(cx, |view, cx| {
            view.open(first.clone(), cx);
            view.open(second.clone(), cx);
            view.tabs[0].log = Some(log_first);
            view.tabs[0].loading = false;
            view.tabs[1].log = Some(log_second);
            view.tabs[1].loading = false;
            view.active = 0;
            cx.notify();
        });
        cx.run_until_parked();

        let strip_bounds = cx.debug_bounds("tab-strip").expect("tab strip bounds");
        let tab1_bounds = cx.debug_bounds("tab-1").expect("tab 1 bounds");
        assert!(
            strip_bounds.right() > tab1_bounds.right() + px(8.),
            "expected empty drag area to the right of open tabs"
        );
        let ribbon_open_pt = point(strip_bounds.right() - px(4.), strip_bounds.center().y);
        simulate_double_click(cx, ribbon_open_pt);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 4));

        // Double-clicking a tab switches to it and does not maximize.
        let tab1_body = point(tab1_bounds.left() + px(12.), tab1_bounds.center().y);
        simulate_double_click(cx, tab1_body);
        view.read_with(cx, |view, _| {
            assert_eq!(view.active, 1);
            assert_eq!(view.header_zoom_count, 4);
        });

        // Double-clicking an inline header action button toggles it without maximizing.
        let follow_btn = cx
            .debug_bounds("header-follow")
            .expect("follow button bounds")
            .center();
        let initial_follow = view.read_with(cx, |view, _| view.tabs[view.active].follow);
        simulate_double_click(cx, follow_btn);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[view.active].follow, initial_follow);
            assert_eq!(view.header_zoom_count, 4);
        });

        // Clicking outside the header disarms a prior single header click.
        let header_bounds = cx.debug_bounds("header").expect("header bounds");
        let outside_pt = point(header_bounds.center().x, header_bounds.bottom() + px(40.));
        simulate_counted_click(cx, ribbon_open_pt, MouseButton::Left, 1);
        simulate_counted_click(cx, outside_pt, MouseButton::Left, 1);
        simulate_counted_click(cx, ribbon_open_pt, MouseButton::Left, 2);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 4));

        // Pressing on the header and releasing outside does not arm or maximize.
        cx.simulate_mouse_down(ribbon_open_pt, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(outside_pt, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(outside_pt, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        simulate_counted_click(cx, ribbon_open_pt, MouseButton::Left, 2);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 4));

        // Rapidly closing the last tab via its close button does not maximize on the second click
        // even though the second click lands on the newly exposed empty ribbon space.
        view.update(cx, |view, cx| view.close_tab_at(1, cx));
        cx.run_until_parked();
        let last_close = cx
            .debug_bounds("tab-close-0")
            .expect("last tab close button bounds")
            .center();
        simulate_double_click(cx, last_close);
        view.read_with(cx, |view, _| {
            assert!(view.tabs.is_empty());
            assert_eq!(view.header_zoom_count, 4);
        });

        // Double-clicking the top ribbon while in fullscreen is ignored.
        view.update_in(cx, |_view, window, _cx| window.toggle_fullscreen());
        cx.run_until_parked();
        let fullscreen_strip = cx
            .debug_bounds("tab-strip")
            .expect("fullscreen tab strip bounds")
            .center();
        simulate_double_click(cx, fullscreen_strip);
        view.read_with(cx, |view, _| assert_eq!(view.header_zoom_count, 4));
    }

    #[gpui::test]
    fn zoom_shortcuts_scale_main_and_filter_rows_clamp_bounds_and_reset(cx: &mut TestAppContext) {
        use nox::zoom::{MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT};

        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zoom.log");
        let content: String = (0..120)
            .map(|i| format!("INFO line {i} payload ERROR sample\n"))
            .collect();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::new(config.clone(), Vec::new(), window, cx);
            workspace.open(path.clone(), cx);
            workspace.tabs[0].log = Some(log);
            workspace.tabs[0].loading = false;
            workspace
                .input
                .update(cx, |input, cx| input.emplace("ERROR", cx));
            workspace.start_search(0, cx);
            workspace
        });
        cx.run_until_parked();

        // Shortcut labels format cleanly for Cmd+=, Cmd+-, Cmd+0 (or Ctrl variants).
        view.read_with(cx, |view, _| {
            if cfg!(target_os = "macos") {
                assert_eq!(view.theme.shortcut(Command::ZoomIn).as_deref(), Some("⌘="));
                assert_eq!(view.theme.shortcut(Command::ZoomOut).as_deref(), Some("⌘-"));
                assert_eq!(
                    view.theme.shortcut(Command::ZoomReset).as_deref(),
                    Some("⌘0")
                );
            } else {
                assert_eq!(
                    view.theme.shortcut(Command::ZoomIn).as_deref(),
                    Some("Ctrl+=")
                );
                assert_eq!(
                    view.theme.shortcut(Command::ZoomOut).as_deref(),
                    Some("Ctrl+-")
                );
                assert_eq!(
                    view.theme.shortcut(Command::ZoomReset).as_deref(),
                    Some("Ctrl+0")
                );
            }
            assert_eq!(view.zoom_percent, DEFAULT_ZOOM_PERCENT);
        });

        // Baseline row heights match configured viewer.line_height (24px) and no zoom pill is shown.
        let main_h_100 = f32::from(cx.debug_bounds("main-row-0").unwrap().size.height);
        let result_h_100 = f32::from(cx.debug_bounds("result-row-0").unwrap().size.height);
        assert_eq!(main_h_100, 24.0);
        assert_eq!(result_h_100, 24.0);
        assert!(cx.debug_bounds("status-zoom").is_none());

        // Cmd+= (or Ctrl+=) zooms in to 110%, growing both main and filter rows and showing status pill.
        let zoom_in_key = if cfg!(target_os = "macos") {
            "cmd-="
        } else {
            "ctrl-="
        };
        let zoom_plus_key = if cfg!(target_os = "macos") {
            "cmd-+"
        } else {
            "ctrl-+"
        };
        let zoom_shift_eq_key = if cfg!(target_os = "macos") {
            "cmd-shift-="
        } else {
            "ctrl-shift-="
        };
        let zoom_out_key = if cfg!(target_os = "macos") {
            "cmd--"
        } else {
            "ctrl--"
        };
        let zoom_reset_key = if cfg!(target_os = "macos") {
            "cmd-0"
        } else {
            "ctrl-0"
        };

        cx.simulate_keystrokes(zoom_in_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 110));
        let main_h_110 = f32::from(cx.debug_bounds("main-row-0").unwrap().size.height);
        let result_h_110 = f32::from(cx.debug_bounds("result-row-0").unwrap().size.height);
        assert_eq!(main_h_110, 26.0);
        assert_eq!(result_h_110, 26.0);
        assert!(cx.debug_bounds("status-zoom").is_some());

        // Companion aliases Cmd++ and Cmd+Shift+= also zoom in cleanly.
        cx.simulate_keystrokes(zoom_plus_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 120));
        cx.simulate_keystrokes(zoom_shift_eq_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 130));

        // Cmd+- zooms out one step to 120%.
        cx.simulate_keystrokes(zoom_out_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 120));
        let main_h_120 = f32::from(cx.debug_bounds("main-row-0").unwrap().size.height);
        assert_eq!(main_h_120, 29.0);

        // Clicking the status-bar zoom badge resets zoom to 100% and hides the badge.
        let zoom_pill = cx.debug_bounds("status-zoom").unwrap().center();
        cx.simulate_click(zoom_pill, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 100));
        assert_eq!(
            f32::from(cx.debug_bounds("main-row-0").unwrap().size.height),
            24.0
        );
        assert!(cx.debug_bounds("status-zoom").is_none());

        // Zoom works while the search input is focused without inserting characters into the query.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(zoom_in_key);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.zoom_percent, 110);
            assert_eq!(view.input.read(cx).as_str(), "ERROR");
        });
        cx.simulate_keystrokes(zoom_out_key);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.zoom_percent, 100);
            assert_eq!(view.input.read(cx).as_str(), "ERROR");
        });

        // Clamps cleanly at MAX_ZOOM_PERCENT and MIN_ZOOM_PERCENT, and Cmd+0 resets to 100%.
        for _ in 0..30 {
            cx.simulate_keystrokes(zoom_in_key);
        }
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.zoom_percent, MAX_ZOOM_PERCENT);
        });
        assert_eq!(
            f32::from(cx.debug_bounds("main-row-0").unwrap().size.height),
            72.0
        );

        for _ in 0..40 {
            cx.simulate_keystrokes(zoom_out_key);
        }
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.zoom_percent, MIN_ZOOM_PERCENT);
        });
        assert_eq!(
            f32::from(cx.debug_bounds("main-row-0").unwrap().size.height),
            12.0
        );
        assert_eq!(
            f32::from(cx.debug_bounds("result-row-0").unwrap().size.height),
            12.0
        );

        cx.simulate_keystrokes(zoom_reset_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 100));

        // Explicit user binding on `secondary-+` is preserved and not overridden by the ZoomIn alias.
        let mut custom = config;
        custom.keyboard.bindings.push(config::Binding {
            keys: "secondary-+".into(),
            context: config::KeyContext::Workspace,
            action: Command::Reload,
        });
        let custom_bindings = keybindings(&custom).unwrap();
        let plus = Keystroke::parse("secondary-+").unwrap();
        let plus_actions: Vec<Command> = custom_bindings
            .iter()
            .filter(|b| b.keystrokes().len() == 1 && b.keystrokes()[0].inner() == &plus)
            .filter_map(|b| {
                b.action()
                    .as_any()
                    .downcast_ref::<Execute>()
                    .map(|e| e.command)
            })
            .collect();
        assert_eq!(plus_actions, vec![Command::Reload]);
    }

    #[gpui::test]
    fn zoom_preserves_scroll_position_adapts_page_navigation_and_persists_in_session(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let state_file = directory.path().join("session.json");
        let path = directory.path().join("scroll.log");
        let content: String = (0..300)
            .map(|i| format!("2026-11-25 INFO row-{i:04} message body\n"))
            .collect();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let mut config = Config::load(None).unwrap();
        config.files.save_session = true;
        config.files.restore_session = true;
        config.files.state_file = Some(state_file.display().to_string());
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let (view, vcx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::new(config.clone(), Vec::new(), window, cx);
            workspace.open(path.clone(), cx);
            workspace.tabs[0].log = Some(log);
            workspace.tabs[0].loading = false;
            workspace
        });
        vcx.run_until_parked();

        // Scroll main pane so row 100 sits at the top of the viewport (100 * 24px = 2400px).
        view.update(vcx, |view, cx| {
            view.tabs[0]
                .scroll
                .scroll_to_item_strict(100, ScrollStrategy::Top);
            cx.notify();
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            let offset_y = f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!((offset_y - -2400.0).abs() < 1.0, "got offset_y={offset_y}");
        });

        // Zoom in 5 steps to 150% (line_height = 36px): row 100 stays anchored at top (-3600px).
        view.update_in(vcx, |view, window, cx| {
            for _ in 0..5 {
                view.command(Command::ZoomIn, window, cx);
            }
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            assert_eq!(view.zoom_percent, 150);
            let offset_y = f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(
                (offset_y - -3600.0).abs() < 2.0,
                "expected row 100 anchored near -3600px, got {offset_y}"
            );
        });

        // PageDown moves fewer rows when zoomed in to 200% than at 100%.
        view.update_in(vcx, |view, window, cx| {
            view.command(Command::ZoomReset, window, cx);
            view.tabs[0].selected = 0;
            view.tabs[0].main_selection.select_single(0);
            view.command(Command::PageDown, window, cx);
        });
        vcx.run_until_parked();
        let step_100 = view.read_with(vcx, |view, _| view.tabs[0].selected);

        view.update_in(vcx, |view, window, cx| {
            for _ in 0..10 {
                view.command(Command::ZoomIn, window, cx);
            }
            view.tabs[0].selected = 0;
            view.tabs[0].main_selection.select_single(0);
            view.command(Command::PageDown, window, cx);
        });
        vcx.run_until_parked();
        let step_200 = view.read_with(vcx, |view, _| view.tabs[0].selected);
        assert!(
            step_200 > 0 && step_200 < step_100,
            "expected smaller page step at 200% zoom (100%={step_100}, 200%={step_200})"
        );

        // Following mode stays pinned to the bottom when zooming in/out.
        view.update_in(vcx, |view, window, cx| {
            view.tabs[0].follow = true;
            view.tabs[0].scroll.scroll_to_bottom();
            view.command(Command::ZoomOut, window, cx);
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            assert_eq!(view.zoom_percent, 190);
            assert_eq!(view.tabs[0].scroll.is_scrolled_to_end(), Some(true));
        });

        // Session persistence saves and restores zoom_percent, and omits it when reset to 100%.
        view.read_with(vcx, |view, cx| view.save_session(cx));
        let saved = Session::load(&state_file, &config).unwrap();
        assert_eq!(saved.zoom_percent, Some(190));

        let (restored_view, restored_cx) =
            cx.add_window_view(|window, cx| Workspace::new(config.clone(), Vec::new(), window, cx));
        restored_cx.run_until_parked();
        restored_view.read_with(restored_cx, |view, _| {
            assert_eq!(view.zoom_percent, 190);
        });

        restored_view.update_in(restored_cx, |view, window, cx| {
            view.command(Command::ZoomReset, window, cx);
            view.save_session(cx);
        });
        let saved_reset = Session::load(&state_file, &config).unwrap();
        assert_eq!(saved_reset.zoom_percent, None);
    }

    #[gpui::test]
    fn header_zoom_buttons_adjust_zoom_stay_stable_clamp_and_never_maximize_window(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("buttons.log");
        std::fs::write(&path, "INFO alpha\nERROR beta\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, Vec::new(), window, cx));
        cx.run_until_parked();

        // Zoom buttons are hidden on the empty welcome screen.
        assert!(cx.debug_bounds("zoom-in").is_none());
        assert!(cx.debug_bounds("zoom-out").is_none());

        // Opening a tab shows both zoom buttons in the top toolbar.
        view.update(cx, |view, cx| {
            view.open(path.clone(), cx);
            view.tabs[0].log = Some(log);
            view.tabs[0].loading = false;
            cx.notify();
        });
        cx.run_until_parked();

        let zoom_in_bounds = cx.debug_bounds("zoom-in").expect("zoom-in button bounds");
        let zoom_out_bounds = cx.debug_bounds("zoom-out").expect("zoom-out button bounds");
        assert!(
            zoom_out_bounds.right() <= zoom_in_bounds.left(),
            "zoom-out should sit to the left of zoom-in"
        );

        // Clicking zoom-in grows zoom to 110% without shifting button coordinates.
        cx.simulate_click(zoom_in_bounds.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 110));
        assert_eq!(
            cx.debug_bounds("zoom-in").expect("zoom-in after click"),
            zoom_in_bounds
        );
        assert_eq!(
            cx.debug_bounds("zoom-out").expect("zoom-out after click"),
            zoom_out_bounds
        );

        // Rapid double-clicking zoom-in zooms twice without maximizing the window.
        let pos = zoom_in_bounds.center();
        for count in [1, 2] {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position: pos,
                modifiers: Modifiers::none(),
                click_count: count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position: pos,
                modifiers: Modifiers::none(),
                click_count: count,
            });
            cx.run_until_parked();
        }
        view.read_with(cx, |view, _| {
            assert_eq!(view.zoom_percent, 130);
            assert_eq!(view.header_zoom_count, 0);
        });

        // Clicking zoom-out steps back down.
        cx.simulate_click(zoom_out_bounds.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.zoom_percent, 120));

        // At MAX_ZOOM_PERCENT, zoom-in is disabled (no-op); at MIN_ZOOM_PERCENT, zoom-out is disabled.
        view.update(cx, |view, cx| {
            view.set_zoom_percent(MAX_ZOOM_PERCENT, cx);
        });
        cx.run_until_parked();
        cx.simulate_click(zoom_in_bounds.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.zoom_percent, MAX_ZOOM_PERCENT);
            assert_eq!(view.header_zoom_count, 0);
        });

        view.update(cx, |view, cx| {
            view.set_zoom_percent(MIN_ZOOM_PERCENT, cx);
        });
        cx.run_until_parked();
        cx.simulate_click(zoom_out_bounds.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.zoom_percent, MIN_ZOOM_PERCENT);
            assert_eq!(view.header_zoom_count, 0);
        });
    }
}
