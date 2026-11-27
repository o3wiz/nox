mod components;
mod icons;
mod resize;
mod scrollbar;
mod selectable_text;
mod sticky_row;
mod theme;

use components::{
    ButtonTone, TOOLTIP_DELAY, Tooltip, format_bytes, format_count, icon_button, search_toggle,
    toolbar_divider,
};
use gpui::{prelude::*, *};
use gpui_ce_elements::editable_text::{
    EditableTextState, StringStorage, TextBoundary, TextChanged,
    actions::{
        self as input_actions, DEFAULT_INPUT_CONTEXT, EditableTextActionHandler, default_bindings,
    },
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
    search::{self, MatchCopyResult, SearchResults},
    selection::{LineSelection, TextPosition, TextSelection, word_range_at},
    session::{self, SavedTab, Session},
    stdin::StdinSpool,
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
    DEFAULT_SCROLLBAR_WIDTH, HORIZONTAL_BAR_HEIGHT, RevealOutcome, RevealStrategy, ScrollAxis,
    ScrollbarTrackState, horizontal_scroll_step, resolve_track_press, reveal_uniform_list_row,
    scroll_offset_for_drag, scrollbar_canvas, set_uniform_list_axis_offset,
    step_uniform_list_axis_offset,
};
use selectable_text::{RowLayoutCache, SelectableRowText, TextPressEvent};
use serde::Deserialize;
use std::{
    cell::Cell,
    collections::{BTreeSet, HashSet, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};
use sticky_row::StickyRowContent;
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

/// Returns GPUI CE's default editable-text keybindings with built-in line-deletion
/// shortcuts (`DeleteToLineStart` / `DeleteToLineEnd`) stripped so those actions only
/// trigger when explicitly configured in `keyboard.json`.
fn text_input_default_bindings() -> Vec<KeyBinding> {
    default_bindings()
        .as_keybindings(Some(DEFAULT_INPUT_CONTEXT))
        .filter(|binding| {
            !binding
                .action()
                .partial_eq(&input_actions::DeleteToLineStart)
                && !binding.action().partial_eq(&input_actions::DeleteToLineEnd)
        })
        .collect()
}

fn sanitize_single_line_search_text(text: &str) -> String {
    text.replace(['\r', '\n'], "")
}

fn ends_with_unescaped_pipe(pattern: &str) -> bool {
    let Some(prefix) = pattern.strip_suffix('|') else {
        return false;
    };
    let trailing_backslashes = prefix.bytes().rev().take_while(|&b| b == b'\\').count();
    trailing_backslashes.is_multiple_of(2)
}

fn append_regex_alternative_with_offset(existing: &str, addition: &str) -> (String, usize) {
    if existing.is_empty() {
        (addition.to_owned(), 0)
    } else if ends_with_unescaped_pipe(existing) {
        (format!("{existing}{addition}"), existing.len())
    } else {
        (format!("{existing}|{addition}"), existing.len() + 1)
    }
}

fn append_regex_alternative(existing: &str, selected: &str) -> String {
    append_regex_alternative_with_offset(existing, selected).0
}

fn utf16_range_for_utf8(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let start = text[..range.start].encode_utf16().count();
    let end = start + text[range.start..range.end].encode_utf16().count();
    start..end
}

const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BULK_BOOKMARK_LINES: usize = 100_000;
const SEARCH_INDICATOR_DELAY: Duration = Duration::from_millis(150);
const MAX_CLOSED_TABS: usize = 50;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingReveal {
    row: usize,
    strategy: RevealStrategy,
}

pub enum InputSource {
    File(PathBuf),
    Stdin(StdinSpool),
}

struct Tab {
    id: usize,
    path: PathBuf,
    log: Option<Arc<LogFile>>,
    stdin: Option<StdinSpool>,
    stdin_finished: bool,
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
    pending_main_reveal: Option<PendingReveal>,
    pending_results_reveal: Option<PendingReveal>,
    error: Option<String>,
}

impl Tab {
    fn new(id: usize, path: PathBuf, options: SearchConfig, follow: bool) -> Self {
        Self {
            id,
            path,
            log: None,
            stdin: None,
            stdin_finished: false,
            loading: false,
            searching: false,
            searching_since: None,
            searching_visible: false,
            load_cancel: Arc::new(AtomicBool::new(false)),
            search_cancel: Arc::new(AtomicBool::new(false)),
            revision: 0,
            query: String::new(),
            options,
            highlight_regex: None,
            results: SearchResults::default(),
            bookmarks: BTreeSet::new(),
            selected: 0,
            main_selection: LineSelection::single(0),
            result_selection: LineSelection::empty(),
            main_text_selection: None,
            result_text_selection: None,
            follow,
            scroll: UniformListScrollHandle::new(),
            result_scroll: UniformListScrollHandle::new(),
            pending_main_reveal: None,
            pending_results_reveal: None,
            error: None,
        }
    }

    fn to_saved(&self, query: String) -> SavedTab {
        SavedTab {
            path: self.path.clone(),
            query,
            options: self.options.clone(),
            bookmarks: self.bookmarks.clone(),
            selected: self.selected,
            follow: self.follow,
        }
    }

    fn apply_saved(&mut self, saved: SavedTab) {
        self.query = saved.query;
        self.options = saved.options;
        self.sync_highlight_regex();
        self.bookmarks = saved.bookmarks;
        self.selected = saved.selected;
        self.main_selection = LineSelection::single(saved.selected);
        self.follow = saved.follow;
    }

    fn is_stdin(&self) -> bool {
        self.stdin.is_some()
    }

    fn is_streaming_stdin(&self) -> bool {
        self.stdin
            .as_ref()
            .is_some_and(|spool| !spool.is_finished())
    }

    fn label(&self) -> String {
        if self.is_stdin() {
            "stdin".to_owned()
        } else {
            file_label(&self.path)
        }
    }

    fn tooltip(&self) -> String {
        if self.is_stdin() {
            if self.is_streaming_stdin() {
                "Standard input (streaming…)".to_owned()
            } else {
                "Standard input".to_owned()
            }
        } else {
            self.path.display().to_string()
        }
    }

    fn path_bar_text(&self) -> String {
        if self.is_stdin() {
            String::new()
        } else {
            self.path.display().to_string()
        }
    }

    fn path_bar_placeholder(&self) -> &'static str {
        if self.is_stdin() {
            if self.is_streaming_stdin() {
                "Standard input (streaming…)"
            } else {
                "Standard input"
            }
        } else {
            "Enter file path…"
        }
    }

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

struct ClosedTab {
    index: usize,
    saved: SavedTab,
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
    closed_tabs: VecDeque<ClosedTab>,
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
    pending_reveal_frame_scheduled: bool,
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
    ephemeral_only: bool,
    recent: Vec<PathBuf>,
    show_files: bool,
    header_click_armed: bool,
    #[cfg(test)]
    header_zoom_count: usize,
    #[cfg(test)]
    quit_count: usize,
}

impl Workspace {
    #[cfg(test)]
    fn new(
        config: Config,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_inputs(
            config,
            paths.into_iter().map(InputSource::File).collect(),
            window,
            cx,
        )
    }

    fn new_with_inputs(
        config: Config,
        inputs: Vec<InputSource>,
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
        let restore = inputs.is_empty() && config.files.restore_session;
        let ephemeral_only = !inputs.is_empty()
            && inputs
                .iter()
                .all(|input| matches!(input, InputSource::Stdin(_)));
        let inputs = if restore {
            saved
                .tabs
                .iter()
                .map(|tab| InputSource::File(tab.path.clone()))
                .collect()
        } else {
            inputs
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
            cx.on_focus_lost(window, |this, window, cx| {
                this.restore_focus(window, cx);
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
            closed_tabs: VecDeque::new(),
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
            pending_reveal_frame_scheduled: false,
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
            ephemeral_only,
            recent: saved.recent,
            show_files: false,
            header_click_armed: false,
            #[cfg(test)]
            header_zoom_count: 0,
            #[cfg(test)]
            quit_count: 0,
        };
        for input in inputs {
            match input {
                InputSource::File(path) => this.open(path, cx),
                InputSource::Stdin(spool) => this.open_stdin(spool, cx),
            }
        }
        if restore {
            for (tab, saved) in this.tabs.iter_mut().zip(saved.tabs) {
                tab.apply_saved(saved);
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
        } else {
            let warnings = this.config.warnings();
            if warnings.len() == 1 {
                this.status = Some(StatusMessage::warning(warnings[0].clone()));
            } else if !warnings.is_empty() {
                this.status = Some(StatusMessage::warning(format!(
                    "{} config warnings: {}",
                    warnings.len(),
                    warnings[0]
                )));
            }
        }
        this
    }

    fn save_session(&self, cx: &App) {
        if !self.config.files.save_session || self.ephemeral_only {
            return;
        }
        let Some(path) = &self.state_path else {
            return;
        };
        let mut persisted_tabs = Vec::new();
        let mut persisted_active = 0;
        for (index, tab) in self.tabs.iter().enumerate() {
            if tab.is_stdin() {
                continue;
            }
            if index == self.active {
                persisted_active = persisted_tabs.len();
            }
            let query = if index == self.active {
                self.input.read(cx).as_str().to_owned()
            } else {
                tab.query.clone()
            };
            persisted_tabs.push(tab.to_saved(query));
        }
        if !self.tabs.is_empty() && persisted_tabs.is_empty() {
            return;
        }
        let session = Session {
            version: 1,
            active: persisted_active,
            recent: self.recent.clone(),
            results_height: Some(self.target_results_height.round()),
            zoom_percent: (self.zoom_percent != DEFAULT_ZOOM_PERCENT).then_some(self.zoom_percent),
            tabs: persisted_tabs,
        };
        if let Err(error) = session.save(path) {
            eprintln!("Cannot save session {}: {error}", path.display());
        }
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = path.canonicalize().unwrap_or(path);
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| !tab.is_stdin() && tab.path == path)
        {
            self.switch(index, cx);
            return;
        }
        self.ephemeral_only = false;
        self.recent.retain(|recent| recent != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(self.config.files.recent_limit);
        self.show_files = false;
        self.next_id += 1;
        self.tabs.push(Tab::new(
            self.next_id,
            path,
            self.config.search.clone(),
            self.config.viewer.follow,
        ));
        self.switch(self.tabs.len() - 1, cx);
        self.load(self.active);
        cx.notify();
    }

    fn open_stdin(&mut self, spool: StdinSpool, cx: &mut Context<Self>) {
        let path = spool.path().to_path_buf();
        let stdin_finished = spool.is_finished();
        self.show_files = false;
        self.next_id += 1;
        let mut tab = Tab::new(
            self.next_id,
            path,
            self.config.search.clone(),
            self.config.viewer.follow,
        );
        tab.stdin = Some(spool);
        tab.stdin_finished = stdin_finished;
        self.tabs.push(tab);
        self.switch(self.tabs.len() - 1, cx);
        self.load(self.active);
        cx.notify();
    }

    fn switch(&mut self, index: usize, cx: &mut Context<Self>) {
        self.activate_tab(index, true, cx);
    }

    fn activate_tab(&mut self, index: usize, save_current_query: bool, cx: &mut Context<Self>) {
        if save_current_query && let Some(tab) = self.tabs.get_mut(self.active) {
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

    fn record_closed_tab(&mut self, index: usize, tab: &Tab) {
        if tab.is_stdin() {
            return;
        }
        self.closed_tabs
            .retain(|closed| closed.saved.path != tab.path);
        if self.closed_tabs.len() >= MAX_CLOSED_TABS {
            self.closed_tabs.pop_front();
        }
        self.closed_tabs.push_back(ClosedTab {
            index,
            saved: tab.to_saved(tab.query.clone()),
        });
    }

    fn restore_closed_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = loop {
            let Some(entry) = self.closed_tabs.pop_back() else {
                return false;
            };
            if !self
                .tabs
                .iter()
                .any(|tab| !tab.is_stdin() && tab.path == entry.saved.path)
            {
                break entry;
            }
        };
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.query = self.input.read(cx).as_str().to_string();
        }
        let path = closed.saved.path.clone();
        self.ephemeral_only = false;
        self.recent.retain(|recent| recent != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(self.config.files.recent_limit);
        self.show_files = false;
        self.show_goto = false;
        self.next_id += 1;
        let mut tab = Tab::new(
            self.next_id,
            path,
            self.config.search.clone(),
            self.config.viewer.follow,
        );
        tab.apply_saved(closed.saved);
        let insert_index = closed.index.min(self.tabs.len());
        self.tabs.insert(insert_index, tab);
        self.activate_tab(insert_index, false, cx);
        self.load(insert_index);
        true
    }

    fn close_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
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
        let removed = self.tabs.remove(index);
        self.record_closed_tab(index, &removed);
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
        self.ephemeral_only = false;
        self.recent.retain(|recent| recent != &path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(self.config.files.recent_limit);
        self.show_files = false;
        self.dismiss_presets(cx);
        self.show_goto = false;
        self.close_path_completions();

        let active = self.active;
        if !self.tabs[active].is_stdin() && self.tabs[active].path == path {
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
        tab.stdin = None;
        tab.stdin_finished = false;
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
        tab.pending_main_reveal = None;
        tab.pending_results_reveal = None;
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
        tab.error = tab.stdin.as_ref().and_then(StdinSpool::error);
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
                tab.pending_results_reveal = None;
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
                    tab.pending_results_reveal = None;
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
        let line_height = self.zoom_metrics().line_height;
        let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                                let non_empty = !log.is_empty();
                                tab.log = Some(log);
                                tab.error = tab.stdin.as_ref().and_then(StdinSpool::error);
                                if tab.follow {
                                    tab.pending_main_reveal = None;
                                    tab.scroll.scroll_to_bottom();
                                } else if initial && non_empty {
                                    Self::reveal_main_row_in_tab(
                                        tab,
                                        tab.selected,
                                        line_height,
                                        surrounding_lines,
                                        RevealStrategy::Center,
                                    );
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
        for index in 0..self.tabs.len() {
            let mut reload_stdin_now = false;
            {
                let tab = &mut self.tabs[index];
                if tab.searching
                    && !tab.searching_visible
                    && tab
                        .searching_since
                        .is_some_and(|since| since.elapsed() >= SEARCH_INDICATOR_DELAY)
                {
                    tab.searching_visible = true;
                    changed = true;
                }
                if let Some(spool) = &tab.stdin {
                    let finished = spool.is_finished();
                    if finished && !tab.stdin_finished {
                        tab.stdin_finished = true;
                        changed = true;
                    }
                    if let Some(error) = spool.error()
                        && tab.error.is_none()
                    {
                        tab.error = Some(error);
                        changed = true;
                    }
                    if !tab.loading
                        && !tab.searching
                        && let Some(log) = &tab.log
                    {
                        let written = spool.bytes_written();
                        if written > log.stamp.len && (log.stamp.len == 0 || finished) {
                            reload_stdin_now = true;
                        }
                    }
                }
            }
            if reload_stdin_now {
                self.load(index);
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
                    if let Some(spool) = &tab.stdin {
                        if !spool.is_finished() && spool.bytes_written() > log.stamp.len {
                            self.load(index);
                            changed = true;
                        }
                    } else {
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
        let default_dir = if tab.is_stdin() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            log.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        let picker = cx.prompt_for_new_path(&default_dir, Some("nox-results.log"));
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
        let pane = if self.tabs.is_empty() {
            Pane::Main
        } else {
            pane
        };
        self.active_pane = pane;
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.dismiss_path_editing(cx);
        match pane {
            Pane::Main => self.focus.focus(window, cx),
            Pane::Results => {
                let line_height = self.zoom_metrics().line_height;
                let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                    Self::reveal_results_row_in_tab(
                        tab,
                        row,
                        line_height,
                        surrounding_lines,
                        RevealStrategy::Nearest,
                    );
                }
                self.results_focus.focus(window, cx);
            }
        }
        cx.notify();
    }

    fn focus_visible_pane(&mut self, preferred: Pane, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            self.focus_pane(Pane::Main, window, cx);
            return;
        }
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

    fn restore_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_visible_pane(self.active_pane, window, cx);
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

    fn reveal_main_row_in_tab(
        tab: &mut Tab,
        row: usize,
        line_height: f32,
        surrounding_lines: usize,
        strategy: RevealStrategy,
    ) {
        let Some(log) = &tab.log else {
            tab.pending_main_reveal = None;
            return;
        };
        let len = log.len();
        if len == 0 {
            tab.pending_main_reveal = None;
            return;
        }
        let target = row.min(len - 1);
        match reveal_uniform_list_row(
            &tab.scroll,
            target,
            len,
            line_height,
            surrounding_lines,
            strategy,
        ) {
            RevealOutcome::Deferred => {
                tab.pending_main_reveal = Some(PendingReveal {
                    row: target,
                    strategy,
                });
            }
            RevealOutcome::Scrolled | RevealOutcome::Unchanged => {
                tab.pending_main_reveal = None;
            }
        }
    }

    fn reveal_results_row_in_tab(
        tab: &mut Tab,
        row: usize,
        line_height: f32,
        surrounding_lines: usize,
        strategy: RevealStrategy,
    ) {
        let len = tab.results.visible.len();
        if len == 0 {
            tab.pending_results_reveal = None;
            return;
        }
        let target = row.min(len - 1);
        match reveal_uniform_list_row(
            &tab.result_scroll,
            target,
            len,
            line_height,
            surrounding_lines,
            strategy,
        ) {
            RevealOutcome::Deferred => {
                tab.pending_results_reveal = Some(PendingReveal {
                    row: target,
                    strategy,
                });
            }
            RevealOutcome::Scrolled | RevealOutcome::Unchanged => {
                tab.pending_results_reveal = None;
            }
        }
    }

    fn flush_pending_reveals(&mut self, cx: &mut Context<Self>) {
        let line_height = self.zoom_metrics().line_height;
        let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let mut changed = false;
        if let Some(pending) = tab.pending_main_reveal {
            let len = tab.log.as_ref().map_or(0, |log| log.len());
            let laid_out = tab.scroll.0.borrow().last_item_size.is_some();
            if len == 0 {
                tab.pending_main_reveal = None;
            } else if laid_out {
                tab.pending_main_reveal = None;
                if reveal_uniform_list_row(
                    &tab.scroll,
                    pending.row,
                    len,
                    line_height,
                    surrounding_lines,
                    pending.strategy,
                ) == RevealOutcome::Scrolled
                {
                    changed = true;
                }
            }
        }
        if let Some(pending) = tab.pending_results_reveal {
            let len = tab.results.visible.len();
            let laid_out = tab.result_scroll.0.borrow().last_item_size.is_some();
            if len == 0 {
                tab.pending_results_reveal = None;
            } else if laid_out {
                tab.pending_results_reveal = None;
                if reveal_uniform_list_row(
                    &tab.result_scroll,
                    pending.row,
                    len,
                    line_height,
                    surrounding_lines,
                    pending.strategy,
                ) == RevealOutcome::Scrolled
                {
                    changed = true;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }

    fn sync_main_to_result_cursor(tab: &mut Tab, line_height: f32, surrounding_lines: usize) {
        if let Some(&source_line) = tab.results.visible.get(tab.result_selection.cursor) {
            tab.selected = source_line;
            tab.main_selection.select_single(source_line);
            tab.main_text_selection = None;
            tab.follow = false;
            Self::reveal_main_row_in_tab(
                tab,
                source_line,
                line_height,
                surrounding_lines,
                RevealStrategy::Center,
            );
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
        let line_height = self.zoom_metrics().line_height;
        let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                        Self::sync_main_to_result_cursor(tab, line_height, surrounding_lines);
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

        let line_height = self.zoom_metrics().line_height;
        let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                        Self::sync_main_to_result_cursor(tab, line_height, surrounding_lines);
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

    fn copy_match_results(&mut self, pane: Pane, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(log) = &tab.log else {
            return;
        };
        if tab.results.matches.is_empty() {
            self.status = Some(StatusMessage::warning("No match results to copy"));
            return;
        }
        if Self::selection_count(tab, pane) == 0 {
            self.status = Some(StatusMessage::warning("No lines selected"));
            return;
        }
        let selected_matches = match pane {
            Pane::Main => search::selected_match_lines(
                &tab.results.matches,
                &tab.main_selection,
                None,
                log.len(),
            ),
            Pane::Results => search::selected_match_lines(
                &tab.results.matches,
                &tab.result_selection,
                Some(&tab.results.visible),
                log.len(),
            ),
        };
        if selected_matches.is_empty() {
            self.status = Some(StatusMessage::warning("No match results in selected lines"));
            return;
        }
        let compiled_fallback;
        let regex = if tab.options.mode == SearchMode::Regex && !tab.options.invert {
            if let Some(re) = tab.highlight_regex.as_ref() {
                Some(re)
            } else if !tab.query.is_empty() {
                compiled_fallback = search::compile(&tab.query, &tab.options).ok();
                compiled_fallback.as_ref()
            } else {
                None
            }
        } else {
            None
        };
        match search::extract_match_results(
            log,
            &selected_matches,
            regex,
            &tab.options,
            MAX_COPY_BYTES,
        ) {
            Ok(MatchCopyResult::Copied { text, count }) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.status = Some(StatusMessage::success(if count == 1 {
                    "Copied 1 match result".to_owned()
                } else {
                    format!("Copied {} match results", format_count(count))
                }));
            }
            Ok(MatchCopyResult::Empty) => {
                self.status = Some(StatusMessage::warning("No highlighted match text to copy"));
            }
            Ok(MatchCopyResult::LimitExceeded) => {
                self.status = Some(StatusMessage::warning(
                    "Match results too large to copy (limit 64 MB)",
                ));
            }
            Err(error) => {
                self.status = Some(StatusMessage::error(error.to_string()));
            }
        }
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
        let line_height = self.zoom_metrics().line_height;
        let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                        Self::sync_main_to_result_cursor(tab, line_height, surrounding_lines);
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
        let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                Self::reveal_main_row_in_tab(
                    tab,
                    target,
                    line_height,
                    surrounding_lines,
                    RevealStrategy::Nearest,
                );
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
                Self::reveal_results_row_in_tab(
                    tab,
                    target,
                    line_height,
                    surrounding_lines,
                    RevealStrategy::Nearest,
                );
                Self::sync_main_to_result_cursor(tab, line_height, surrounding_lines);
            }
        }
        cx.notify();
    }

    fn scroll_horizontal(
        &mut self,
        scroll_right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_for(window);
        self.active_pane = pane;
        let (min_h, max_h) = self.results_height_bounds(window);
        let eff = self.results_height.clamp(min_h, max_h);
        if (pane == Pane::Main && is_main_hidden(eff, max_h))
            || (pane == Pane::Results && is_results_hidden(eff))
        {
            return;
        }
        let zoom_scale = self.zoom_metrics().scale;
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let handle = match pane {
            Pane::Main => {
                if tab.log.as_ref().is_none_or(|log| log.is_empty()) {
                    return;
                }
                &tab.scroll
            }
            Pane::Results => {
                if tab.results.visible.is_empty() {
                    return;
                }
                &tab.result_scroll
            }
        };
        let sign = if scroll_right { 1.0 } else { -1.0 };
        if step_uniform_list_axis_offset(handle, ScrollAxis::Horizontal, |viewport| {
            sign * horizontal_scroll_step(viewport, zoom_scale)
        }) {
            cx.notify();
        }
    }

    fn reset_horizontal_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self.active_pane_for(window);
        self.active_pane = pane;
        let (min_h, max_h) = self.results_height_bounds(window);
        let eff = self.results_height.clamp(min_h, max_h);
        if (pane == Pane::Main && is_main_hidden(eff, max_h))
            || (pane == Pane::Results && is_results_hidden(eff))
        {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let handle = match pane {
            Pane::Main => {
                if tab.log.as_ref().is_none_or(|log| log.is_empty()) {
                    return;
                }
                &tab.scroll
            }
            Pane::Results => {
                if tab.results.visible.is_empty() {
                    return;
                }
                &tab.result_scroll
            }
        };
        let current_x = f32::from(handle.0.borrow().base_handle.offset().x);
        if current_x.abs() > 0.01 {
            set_uniform_list_axis_offset(handle, ScrollAxis::Horizontal, 0.0);
            cx.notify();
        }
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
            Command::AppendPreset => {
                self.append_selected_preset(window, cx);
                return;
            }
            Command::NextLine
            | Command::PreviousLine
            | Command::FirstLine
            | Command::LastLine
            | Command::PageUp
            | Command::PageDown => {
                if self.path_input_focused(window, cx)
                    && matches!(
                        command,
                        Command::NextLine
                            | Command::PreviousLine
                            | Command::FirstLine
                            | Command::LastLine
                    )
                {
                    self.navigate_path_completions(command, cx);
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
            Command::ScrollLeft | Command::ScrollRight => {
                self.scroll_horizontal(command == Command::ScrollRight, window, cx);
                return;
            }
            Command::ResetHorizontalScroll => {
                self.reset_horizontal_scroll(window, cx);
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
                let line_height = self.zoom_metrics().line_height;
                let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                            Self::sync_main_to_result_cursor(tab, line_height, surrounding_lines);
                        }
                    }
                }
            }
            Command::SwapSelectionAnchor => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                let line_height = self.zoom_metrics().line_height;
                let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    match pane {
                        Pane::Main => {
                            let len = tab.log.as_ref().map_or(0, |log| log.len());
                            if len > 0 && tab.main_selection.swap_anchor() {
                                let cursor = tab.main_selection.cursor.min(len - 1);
                                tab.main_selection.cursor = cursor;
                                tab.selected = cursor;
                                tab.follow = false;
                                Self::reveal_main_row_in_tab(
                                    tab,
                                    cursor,
                                    line_height,
                                    surrounding_lines,
                                    RevealStrategy::Nearest,
                                );
                            }
                        }
                        Pane::Results => {
                            let len = tab.results.visible.len();
                            if len > 0 && tab.result_selection.swap_anchor() {
                                let cursor = tab.result_selection.cursor.min(len - 1);
                                tab.result_selection.cursor = cursor;
                                Self::reveal_results_row_in_tab(
                                    tab,
                                    cursor,
                                    line_height,
                                    surrounding_lines,
                                    RevealStrategy::Nearest,
                                );
                                Self::sync_main_to_result_cursor(
                                    tab,
                                    line_height,
                                    surrounding_lines,
                                );
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
                let line_height = self.zoom_metrics().line_height;
                let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                            Self::reveal_results_row_in_tab(
                                tab,
                                row,
                                line_height,
                                surrounding_lines,
                                RevealStrategy::Center,
                            );
                            Self::sync_main_to_result_cursor(tab, line_height, surrounding_lines);
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
                            Self::reveal_main_row_in_tab(
                                tab,
                                line,
                                line_height,
                                surrounding_lines,
                                RevealStrategy::Center,
                            );
                            if let Ok(index) = tab.results.visible.binary_search(&line) {
                                tab.result_selection.select_single(index);
                                tab.result_text_selection = None;
                                Self::reveal_results_row_in_tab(
                                    tab,
                                    index,
                                    line_height,
                                    surrounding_lines,
                                    RevealStrategy::Center,
                                );
                            }
                        }
                    }
                }
            }
            Command::Open => self.choose_file(cx),
            Command::Quit => {
                #[cfg(test)]
                {
                    self.quit_count += 1;
                }
                cx.quit();
                return;
            }
            Command::FocusSearch => {
                if self.tabs.is_empty() {
                    return;
                }
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
                if self.tabs.is_empty() {
                    self.focus.focus(window, cx);
                }
            }
            Command::RestoreClosedTab => {
                self.restore_closed_tab(cx);
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
                        tab.pending_main_reveal = None;
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
                let line_height = self.zoom_metrics().line_height;
                let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                        Self::reveal_main_row_in_tab(
                            tab,
                            target,
                            line_height,
                            surrounding_lines,
                            RevealStrategy::Center,
                        );
                        if let Ok(index) = tab.results.visible.binary_search(&target) {
                            tab.result_selection.select_single(index);
                            tab.result_text_selection = None;
                            Self::reveal_results_row_in_tab(
                                tab,
                                index,
                                line_height,
                                surrounding_lines,
                                RevealStrategy::Center,
                            );
                        }
                    }
                }
            }
            Command::CopySelectedText => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                self.copy_selected_text(pane, cx);
            }
            Command::CopyMatchResults => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                self.copy_match_results(pane, cx);
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
                    let line_height = self.zoom_metrics().line_height;
                    let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        if let (Ok(line), Some(log)) = (line, &tab.log) {
                            if line > 0 && line <= log.len() {
                                tab.selected = line - 1;
                                tab.main_selection.select_single(line - 1);
                                tab.main_text_selection = None;
                                tab.follow = false;
                                Self::reveal_main_row_in_tab(
                                    tab,
                                    line - 1,
                                    line_height,
                                    surrounding_lines,
                                    RevealStrategy::Center,
                                );
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
            Command::MoveToLineStart
            | Command::MoveToLineEnd
            | Command::SelectToLineStart
            | Command::SelectToLineEnd
            | Command::DeleteToLineStart
            | Command::DeleteToLineEnd => {
                if let Some(input) = self.focused_text_input(window, cx) {
                    input.update(cx, |state, cx| match command {
                        Command::MoveToLineStart => {
                            state.nav_line_start(&input_actions::NavLineStart, window, cx);
                        }
                        Command::MoveToLineEnd => {
                            state.nav_line_end(&input_actions::NavLineEnd, window, cx);
                        }
                        Command::SelectToLineStart => {
                            state.select_linear(
                                NavigationDirection::Back,
                                TextBoundary::HardLine,
                                cx,
                            );
                        }
                        Command::SelectToLineEnd => {
                            state.select_linear(
                                NavigationDirection::Forward,
                                TextBoundary::HardLine,
                                cx,
                            );
                        }
                        Command::DeleteToLineStart => {
                            state.delete_to_line_start(
                                &input_actions::DeleteToLineStart,
                                window,
                                cx,
                            );
                        }
                        Command::DeleteToLineEnd => {
                            state.delete_to_line_end(&input_actions::DeleteToLineEnd, window, cx);
                        }
                        _ => unreachable!(),
                    });
                }
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

    fn focused_text_input(&self, window: &Window, cx: &App) -> Option<Entity<EditableTextState>> {
        [
            &self.input,
            &self.path_input,
            &self.goto,
            &self.presets_search,
        ]
        .into_iter()
        .find(|input| input.read(cx).focus_handle(cx).is_focused(window))
        .cloned()
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
            .map(Tab::path_bar_text)
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

    fn navigate_path_completions(&mut self, command: Command, cx: &mut Context<Self>) {
        if !self.show_path_completions {
            self.refresh_path_completions(true, cx);
            if !self.path_completions.items.is_empty() {
                let idx = match command {
                    Command::NextLine | Command::FirstLine => 0,
                    Command::PreviousLine | Command::LastLine => {
                        self.path_completions.items.len() - 1
                    }
                    _ => return,
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
        let next = match (self.path_completion_index, command) {
            (_, Command::FirstLine) => 0,
            (_, Command::LastLine) => len - 1,
            (None, Command::NextLine) => 0,
            (None, Command::PreviousLine) => len - 1,
            (Some(i), Command::NextLine) => (i + 1) % len,
            (Some(i), Command::PreviousLine) => (i + len - 1) % len,
            (Some(i), _) => i.min(len - 1),
            (None, _) => return,
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

    fn append_selected_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.show_presets {
            return;
        }
        if let Some(matched) = self.filtered_presets.get(self.preset_index) {
            self.append_preset(matched.index, window, cx);
        }
    }

    fn finish_preset_to_search(
        &mut self,
        pattern: String,
        select_range: Option<std::ops::Range<usize>>,
        has_select_text: bool,
        select_warning: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if has_select_text {
            if let Some(range) = select_range {
                let version = self.input.update(cx, |input, cx| {
                    input.emplace(&pattern, cx);
                    input.move_to(range.start, cx);
                    input.select_to(range.end, cx);
                    input.version()
                });
                self.input.read(cx).focus_handle(cx).focus(window, cx);
                let expected_utf16 = utf16_range_for_utf8(&pattern, range.clone());
                cx.on_next_frame(window, move |this, window, cx| {
                    if !this.input.read(cx).focus_handle(cx).is_focused(window) {
                        return;
                    }
                    let unchanged = this.input.update(cx, |input, cx| {
                        if input.version() != version {
                            return false;
                        }
                        input
                            .selected_text_range(false, window, cx)
                            .is_some_and(|sel| sel.range == expected_utf16)
                    });
                    if unchanged {
                        this.input.update(cx, |input, cx| {
                            input.move_to(range.start, cx);
                            input.select_to(range.end, cx);
                        });
                    }
                });
            } else {
                self.input.update(cx, |input, cx| {
                    input.emplace(&pattern, cx);
                    input.move_to(pattern.len(), cx);
                });
                self.input.read(cx).focus_handle(cx).focus(window, cx);
                if let Some(warning) = select_warning {
                    self.status = Some(StatusMessage::warning(warning));
                }
            }
            cx.notify();
            return;
        }
        self.input
            .update(cx, |input, cx| input.emplace(&pattern, cx));
        self.start_search(self.active, cx);
        self.focus_visible_pane(self.active_pane, window, cx);
    }

    fn apply_preset(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preset) = self.config.search.presets.get(index).cloned() else {
            return;
        };
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.options.mode = preset.mode;
            tab.options.highlight_matches = preset.highlight_match;
        }
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.preset_index = index;
        let select_range = preset.select_text_range();
        let has_select_text = preset.select_text.is_some();
        let select_warning = preset.select_text_warning();
        self.finish_preset_to_search(
            preset.pattern,
            select_range,
            has_select_text,
            select_warning,
            window,
            cx,
        );
    }

    fn append_preset(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.options.mode != SearchMode::Regex {
            return;
        }
        let Some(preset) = self.config.search.presets.get(index).cloned() else {
            return;
        };
        if preset.pattern.is_empty() {
            self.status = Some(StatusMessage::warning(format!(
                "Preset {:?} has an empty pattern",
                preset.name
            )));
            self.dismiss_presets(cx);
            cx.notify();
            return;
        }
        let existing = self.input.read(cx).as_str().to_owned();
        if existing.is_empty()
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.options.highlight_matches = preset.highlight_match;
        }
        let (addition, raw_select_range) = preset.regex_pattern_and_select_range();
        let (combined, offset) = append_regex_alternative_with_offset(&existing, &addition);
        let shifted_range =
            raw_select_range.map(|range| (range.start + offset)..(range.end + offset));
        self.show_goto = false;
        self.dismiss_presets(cx);
        self.preset_index = index;
        let has_select_text = preset.select_text.is_some();
        let select_warning = preset.select_text_warning();
        self.finish_preset_to_search(
            combined,
            shifted_range,
            has_select_text,
            select_warning,
            window,
            cx,
        );
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
                let line_height = self.zoom_metrics().line_height;
                let surrounding_lines = self.config.viewer.cursor_surrounding_lines;
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
                    Self::reveal_results_row_in_tab(
                        tab,
                        row,
                        line_height,
                        surrounding_lines,
                        RevealStrategy::Nearest,
                    );
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
            self.config.window.filter_resize_sensitivity,
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
            if id.axis == ScrollAxis::Vertical {
                match id.pane {
                    Pane::Main => {
                        tab.follow = false;
                        tab.pending_main_reveal = None;
                    }
                    Pane::Results => {
                        tab.pending_results_reveal = None;
                    }
                }
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
            if drag.id.axis == ScrollAxis::Vertical {
                match drag.id.pane {
                    Pane::Main => {
                        tab.follow = false;
                        tab.pending_main_reveal = None;
                    }
                    Pane::Results => {
                        tab.pending_results_reveal = None;
                    }
                }
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
                            let line_height = this.zoom_metrics().line_height;
                            let surrounding_lines = this.config.viewer.cursor_surrounding_lines;
                            if let Some(tab) = this.tabs.get_mut(this.active) {
                                tab.selected = line;
                                tab.main_selection.select_single(line);
                                tab.main_text_selection = None;
                                tab.follow = false;
                                Self::reveal_main_row_in_tab(
                                    tab,
                                    line,
                                    line_height,
                                    surrounding_lines,
                                    RevealStrategy::Center,
                                );
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
                0.0,
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
        let gutter_width = self
            .zoom_metrics()
            .sticky_gutter_width(self.config.viewer.line_numbers);
        let view = cx.entity().downgrade();

        div()
            .debug_selector(move || debug_name.into())
            .absolute()
            .bottom_0()
            .left(px(gutter_width))
            .right_0()
            .h(px(HORIZONTAL_BAR_HEIGHT))
            .child(scrollbar_canvas(
                scroll,
                ScrollAxis::Horizontal,
                gutter_width,
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
        let show_line_numbers = self.config.viewer.line_numbers;
        let scrolled_horizontally = {
            let handle = if results {
                &tab.result_scroll
            } else {
                &tab.scroll
            };
            f32::from(handle.0.borrow().base_handle.offset().x) < -0.5
        };
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

                let mark_border_color = if !show_line_numbers && scrolled_horizontally {
                    t.border.opacity(0.8)
                } else {
                    gpui::transparent_black()
                };
                let line_number_border_color = if scrolled_horizontally {
                    t.border
                } else {
                    t.border.opacity(0.6)
                };
                let sticky_gutter = div()
                    .debug_selector(move || {
                        format!("{}-gutter-{row}", if results { "result" } else { "main" })
                    })
                    .flex()
                    .items_center()
                    .h_full()
                    .flex_shrink_0()
                    .border_l_2()
                    .border_color(border_color)
                    .child(
                        div()
                            .id((if results { "rmark" } else { "mark" }, line))
                            .debug_selector(move || {
                                format!("{}-mark-{row}", if results { "result" } else { "main" })
                            })
                            .w(px(metrics.mark_gutter_width))
                            .h_full()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(!show_line_numbers, |g| {
                                g.border_r_1().border_color(mark_border_color)
                            })
                            .when(marked, |g| {
                                g.child(
                                    Icon::BookmarkFilled
                                        .element(px(metrics.bookmark_icon_size), t.warning),
                                )
                            })
                            .when(!marked && matched, |g| {
                                g.child(
                                    Icon::MatchDot.element(px(metrics.match_dot_size), t.accent),
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
                    .when(show_line_numbers, |gutter| {
                        gutter.child(
                            div()
                                .debug_selector(move || {
                                    format!(
                                        "{}-linenum-{row}",
                                        if results { "result" } else { "main" }
                                    )
                                })
                                .w(px(metrics.line_number_width))
                                .h_full()
                                .pr(px(metrics.line_number_padding))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_end()
                                .border_r_1()
                                .border_color(line_number_border_color)
                                .text_size(px(metrics.line_number_size))
                                .text_color(if selected { t.foreground } else { t.muted })
                                .child(format!("{}", line + 1)),
                        )
                    })
                    .into_any_element();
                let text_cell = div()
                    .flex_shrink_0()
                    .when(show_line_numbers, |cell| {
                        cell.pl(px(metrics.line_number_padding))
                    })
                    .pr_4()
                    .child(
                        div()
                            .debug_selector(move || {
                                format!("{}-text-{row}", if results { "result" } else { "main" })
                            })
                            .child(line_content),
                    )
                    .into_any_element();

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
                        .text_color(text_color)
                        .cursor_pointer()
                        .child(StickyRowContent::new(sticky_gutter, text_cell, row_height))
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
            let filename = tab.label();
            let full_path = SharedString::from(tab.tooltip());
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_tab_at(index, cx);
                                if this.tabs.is_empty() {
                                    this.focus.focus(window, cx);
                                }
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
                    .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.is_middle_click() {
                            cx.stop_propagation();
                            this.close_tab_at(index, cx);
                            if this.tabs.is_empty() {
                                this.focus.focus(window, cx);
                            }
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
                            .child(
                                self.cmd_icon(
                                    "copy",
                                    Icon::Copy,
                                    if has_text_selection {
                                        "Copy selected text".to_owned()
                                    } else if selected_count > 1 {
                                        format!(
                                            "Copy {} selected lines",
                                            format_count(selected_count)
                                        )
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
                                )
                                .debug_selector(|| "copy".into()),
                            )
                            .child(
                                self.cmd_icon(
                                    "copy-matches",
                                    Icon::CopyMatches,
                                    if selected_count > 1 {
                                        format!(
                                            "Copy {} selected lines' match results",
                                            format_count(selected_count)
                                        )
                                    } else if selected_count == 1 {
                                        "Copy selected line's match results".to_owned()
                                    } else {
                                        "Copy selected lines' match results".to_owned()
                                    },
                                    Command::CopyMatchResults,
                                    ButtonTone::Ghost,
                                    cx,
                                )
                                .debug_selector(|| "copy-matches".into()),
                            )
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
        let path_placeholder = self
            .tabs
            .get(self.active)
            .map_or("Enter file path…", Tab::path_bar_placeholder);
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
                    this.navigate_path_completions(Command::PreviousLine, cx);
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
                            .placeholder(path_placeholder)
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
            .debug_selector(|| "welcome".into())
            .track_focus(&self.focus)
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
            let regex_mode = options.mode == SearchMode::Regex;
            let append_shortcut = t.shortcut(Command::AppendPreset);
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

                    let pattern_element: AnyElement =
                        if let Some(select_range) = preset.select_text_range() {
                            let select_style = HighlightStyle {
                                color: Some(t.accent),
                                background_color: Some(t.accent_subtle),
                                ..Default::default()
                            };
                            StyledText::new(preset.pattern.clone())
                                .with_highlights(vec![(select_range, select_style)])
                                .into_any_element()
                        } else {
                            preset.pattern.clone().into_any_element()
                        };
                    let select_warning = preset.select_text_warning();
                    let warning_tip_theme = t.clone();

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
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                if regex_mode && event.modifiers().secondary() {
                                    this.append_preset(preset_index, window, cx);
                                } else {
                                    this.apply_preset(preset_index, window, cx);
                                }
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
                                            .flex()
                                            .items_center()
                                            .gap_1p5()
                                            .when_some(select_warning, |el, warning| {
                                                el.child(
                                                    div()
                                                        .id(("preset-warning", preset_index))
                                                        .debug_selector(move || {
                                                            format!("preset-warning-{preset_index}")
                                                        })
                                                        .flex()
                                                        .items_center()
                                                        .child(
                                                            Icon::AlertTriangle
                                                                .element(px(12.), t.warning),
                                                        )
                                                        .tooltip_show_delay(TOOLTIP_DELAY)
                                                        .tooltip(move |_window, cx| {
                                                            Tooltip::view(
                                                                warning.clone(),
                                                                None,
                                                                warning_tip_theme.clone(),
                                                                cx,
                                                            )
                                                        }),
                                                )
                                            })
                                            .child(
                                                div()
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
                                            )
                                            .when(regex_mode, |el| {
                                                let append_tip_theme = t.clone();
                                                let append_tip_shortcut = append_shortcut.clone();
                                                let append_hover_bg = t.accent_subtle;
                                                let append_hover_border = t.accent_border;
                                                let append_hover_fg = t.accent;
                                                let append_group = SharedString::from(format!(
                                                    "preset-append-{preset_index}"
                                                ));
                                                el.child(
                                                    div()
                                                        .id(("preset-append", preset_index))
                                                        .group(append_group.clone())
                                                        .debug_selector(move || {
                                                            format!("preset-append-{preset_index}")
                                                        })
                                                        .size(px(20.))
                                                        .rounded_sm()
                                                        .border_1()
                                                        .border_color(t.border)
                                                        .bg(t.background)
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .cursor_pointer()
                                                        .hover(move |s| {
                                                            s.bg(append_hover_bg)
                                                                .border_color(append_hover_border)
                                                        })
                                                        .child(
                                                            Icon::Plus
                                                                .element(px(11.), t.muted)
                                                                .group_hover(
                                                                    append_group,
                                                                    move |s| {
                                                                        s.text_color(
                                                                            append_hover_fg,
                                                                        )
                                                                    },
                                                                ),
                                                        )
                                                        .tooltip_show_delay(TOOLTIP_DELAY)
                                                        .tooltip(move |_window, cx| {
                                                            Tooltip::view(
                                                                "Append to search (|)",
                                                                append_tip_shortcut.clone(),
                                                                append_tip_theme.clone(),
                                                                cx,
                                                            )
                                                        })
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            |_, _, cx| cx.stop_propagation(),
                                                        )
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                cx.stop_propagation();
                                                                this.append_preset(
                                                                    preset_index,
                                                                    window,
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                )
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .font(t.mono_font_spec())
                                    .text_size(t.xs_size)
                                    .text_color(t.muted)
                                    .child(pattern_element),
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
                                .debug_selector(|| "presets-header-hint".into())
                                .font(t.mono_font_spec())
                                .text_size(px(10.))
                                .text_color(t.muted)
                                .child(if regex_mode {
                                    if let Some(shortcut) = &append_shortcut {
                                        SharedString::from(format!("↑↓ ↵ · {shortcut} +"))
                                    } else {
                                        SharedString::from("↑↓ ↵ · +")
                                    }
                                } else {
                                    SharedString::from("↑↓ ↵")
                                }),
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
            .child(
                self.cmd_icon(
                    "prev-match",
                    Icon::ChevronUp,
                    "Previous match",
                    Command::PreviousMatch,
                    ButtonTone::Ghost,
                    cx,
                )
                .debug_selector(|| "prev-match".into()),
            )
            .child(
                self.cmd_icon(
                    "next-match",
                    Icon::ChevronDown,
                    "Next match",
                    Command::NextMatch,
                    ButtonTone::Ghost,
                    cx,
                )
                .debug_selector(|| "next-match".into()),
            )
            .child(
                self.cmd_icon(
                    "search",
                    Icon::ArrowRight,
                    "Apply search",
                    Command::Search,
                    ButtonTone::Primary,
                    cx,
                )
                .debug_selector(|| "search-apply".into()),
            );

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
        let stdin_suffix = tab
            .and_then(|tab| {
                tab.is_stdin().then_some(if tab.is_streaming_stdin() {
                    " · Streaming"
                } else {
                    " · stdin"
                })
            })
            .unwrap_or("");

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
                                    .child(div().font(t.mono_font_spec()).child(format!(
                                        "{} · UTF-8{stdin_suffix}",
                                        format_bytes(len)
                                    ))),
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
        let needs_pending_reveal = self.tabs.get(self.active).is_some_and(|tab| {
            (tab.pending_main_reveal.is_some() && line_count > 0 && !main_hidden)
                || (tab.pending_results_reveal.is_some() && result_count > 0 && !results_hidden)
        });
        if needs_pending_reveal && !self.pending_reveal_frame_scheduled {
            self.pending_reveal_frame_scheduled = true;
            cx.on_next_frame(window, |this, _window, cx| {
                this.pending_reveal_frame_scheduled = false;
                this.flush_pending_reveals(cx);
            });
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
                        .key_context("FilterLog")
                        .flex_1()
                        .w_full()
                        .min_h_0()
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

pub fn run(config: Config, inputs: Vec<InputSource>) -> anyhow::Result<()> {
    let bindings = keybindings(&config)?;
    gpui_platform::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx: &mut App| {
            // GPUI CE 0.3.0 invokes this callback while holding the macOS
            // platform mutex. Opening a native window here takes that mutex
            // again. Schedule startup after the launch callback has returned.
            cx.spawn(async move |cx| {
                cx.update(move |cx| {
                    cx.bind_keys(text_input_default_bindings());
                    cx.bind_keys(bindings);
                    cx.on_action(|action: &Execute, cx| {
                        if action.command == Command::Quit {
                            cx.quit();
                        }
                    });
                    cx.set_menus([Menu::new("Nox").items([
                        MenuItem::os_submenu("Services", SystemMenuType::Services),
                        MenuItem::separator(),
                        MenuItem::action(
                            "Quit Nox",
                            Execute {
                                command: Command::Quit,
                            },
                        ),
                    ])]);
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
                        cx.new(|cx| Workspace::new_with_inputs(config, inputs, window, cx))
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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

            let visible_pixel_count = |icon: Icon| -> usize {
                cx.svg_renderer()
                    .render_single_frame(icon.svg_bytes(), 2.0)
                    .unwrap()
                    .as_bytes(0)
                    .expect("icon frame bytes")
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|px| px[3] > 0)
                    .count()
            };
            assert!(
                visible_pixel_count(Icon::CopyMatches) > visible_pixel_count(Icon::Copy),
                "Icon::CopyMatches should fill the front square with more opaque pixels than Icon::Copy"
            );
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
        view.update_in(cx, |view, window, cx| {
            view.command(Command::LastLine, window, cx);
        });
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 2));
        cx.simulate_keystrokes("down");
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 0));
        cx.simulate_keystrokes("up");
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 2));
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FirstLine, window, cx);
        });
        cx.simulate_keystrokes("down");
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
            cx.bind_keys(text_input_default_bindings());
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
                select_text: None,
            },
            config::Preset {
                name: "Warnings".into(),
                pattern: "warn".into(),
                mode: SearchMode::Literal,
                highlight_match: true,
                select_text: None,
            },
            config::Preset {
                name: "Requests".into(),
                pattern: "GET|POST|PUT|DELETE".into(),
                mode: SearchMode::Regex,
                highlight_match: true,
                select_text: None,
            },
            config::Preset {
                name: "HTTP 5xx Errors".into(),
                pattern: "HTTP 5\\d\\d".into(),
                mode: SearchMode::Regex,
                highlight_match: true,
                select_text: None,
            },
            config::Preset {
                name: "Auth Errors".into(),
                pattern: "401|403|unauthorized".into(),
                mode: SearchMode::Regex,
                highlight_match: false,
                select_text: None,
            },
        ];
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
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

    #[gpui::test]
    fn applying_selectable_preset_focuses_search_selects_text_and_defers_search(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.search.presets = vec![
            config::Preset {
                name: "Errors".into(),
                pattern: "ERROR".into(),
                mode: SearchMode::Literal,
                highlight_match: true,
                select_text: None,
            },
            config::Preset {
                name: "Port request".into(),
                pattern: "GET .* port PORT_NUMBER".into(),
                mode: SearchMode::Regex,
                highlight_match: false,
                select_text: Some("PORT_NUMBER".into()),
            },
            config::Preset {
                name: "Unicode user".into(),
                pattern: "user=שלום_NAME!".into(),
                mode: SearchMode::Literal,
                highlight_match: true,
                select_text: Some("שלום_NAME".into()),
            },
        ];
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selectable-presets.log");
        std::fs::write(
            &path,
            "ERROR initial failure\nGET /api port 8080\nGET /api port 9090\nuser=alice!\n",
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

        let selected_range = |input: &Entity<EditableTextState>,
                              window: &mut Window,
                              cx: &mut Context<Workspace>| {
            input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range
        };

        // Valid presets produce no startup warning.
        view.read_with(cx, |view, _| assert_eq!(view.status, None));

        // Seed an active search via the non-selectable preset.
        view.update_in(cx, |view, window, cx| view.apply_preset(0, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].query, "ERROR");
            assert_eq!(view.tabs[0].results.matches, vec![0]);
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
            assert!(view.tabs[0].options.highlight_matches);
        });

        let presets_key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };

        // Open patterns dropdown, pick "Port request" with Enter: copies pattern, focuses search,
        // selects "PORT_NUMBER", updates mode/highlight options, and does NOT search yet.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "GET .* port PORT_NUMBER");
            assert_eq!(selected_range(&view.input, window, cx), 12..23);
            assert_eq!(view.tabs[0].options.mode, SearchMode::Regex);
            assert!(!view.tabs[0].options.highlight_matches);
            assert_eq!(view.tabs[0].query, "ERROR");
            assert!(!view.tabs[0].searching);
            assert_eq!(view.tabs[0].results.matches, vec![0]);
            assert_eq!(view.status, None);
        });

        // Typing replaces only the selected placeholder, and pressing Enter executes the search.
        cx.simulate_input("8080");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "GET .* port 8080");
            assert_eq!(view.tabs[0].query, "ERROR");
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].query, "GET .* port 8080");
            assert_eq!(view.tabs[0].results.matches, vec![1]);
        });

        // Clicking a Unicode selectable preset also focuses search, selects the UTF-16 span,
        // defers searching, and allows replacing the placeholder directly.
        let trigger_center = cx
            .debug_bounds("presets-trigger")
            .expect("presets trigger")
            .center();
        cx.simulate_click(trigger_center, Modifiers::none());
        cx.run_until_parked();
        let unicode_item_center = cx
            .debug_bounds("preset-item-2")
            .expect("unicode preset item")
            .center();
        cx.simulate_click(unicode_item_center, Modifiers::none());
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "user=שלום_NAME!");
            let expected =
                utf16_range_for_utf8("user=שלום_NAME!", "user=".len().."user=שלום_NAME".len());
            assert_eq!(selected_range(&view.input, window, cx), expected);
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
            assert!(view.tabs[0].options.highlight_matches);
            assert_eq!(view.tabs[0].query, "GET .* port 8080");
            assert!(!view.tabs[0].searching);
        });
        cx.simulate_input("alice");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "user=alice!");
            assert_eq!(view.tabs[0].query, "user=alice!");
            assert_eq!(view.tabs[0].results.matches, vec![3]);
        });
    }

    #[gpui::test]
    fn selectable_preset_with_missing_select_text_warns_and_focuses_search_without_searching(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.search.presets = vec![
            config::Preset {
                name: "Port request".into(),
                pattern: "GET .* port 8080".into(),
                mode: SearchMode::Regex,
                highlight_match: true,
                select_text: Some("PORT_NUMBER".into()),
            },
            config::Preset {
                name: "Empty select".into(),
                pattern: "warn".into(),
                mode: SearchMode::Literal,
                highlight_match: true,
                select_text: Some(String::new()),
            },
        ];
        cx.update(|cx| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("warn-presets.log");
        std::fs::write(&path, "GET /api port 8080\nwarn slow query\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].main_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        let selected_range = |input: &Entity<EditableTextState>,
                              window: &mut Window,
                              cx: &mut Context<Workspace>| {
            input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range
        };

        // Startup surfaces a warning status for misconfigured select_text presets.
        view.read_with(cx, |view, _| {
            let status = view.status.as_ref().expect("startup warning status");
            assert_eq!(status.tone, StatusTone::Warning);
            assert!(status.text.contains("2 config warnings"));
            assert!(status.text.contains("PORT_NUMBER"));
        });

        // Opening the patterns dropdown renders warning badges on both affected presets.
        view.update_in(cx, |view, window, cx| view.toggle_presets(window, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("preset-warning-0").is_some());
        assert!(cx.debug_bounds("preset-warning-1").is_some());

        // Applying preset 0 warns, focuses search with caret at end, and does not search.
        view.update(cx, |view, _| view.status = None);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "GET .* port 8080");
            assert_eq!(selected_range(&view.input, window, cx), 16..16);
            assert_eq!(view.tabs[0].query, "");
            assert!(!view.tabs[0].searching);
            let status = view.status.as_ref().expect("apply warning status");
            assert_eq!(status.tone, StatusTone::Warning);
            assert!(status.text.contains("Port request"));
            assert!(status.text.contains("PORT_NUMBER"));
        });

        // Applying preset 1 (empty select_text) warns, focuses search at end, and does not search.
        view.update_in(cx, |view, window, cx| {
            view.status = None;
            view.apply_preset(1, window, cx);
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), "warn");
            assert_eq!(selected_range(&view.input, window, cx), 4..4);
            assert_eq!(view.tabs[0].query, "");
            assert!(!view.tabs[0].searching);
            let status = view.status.as_ref().expect("empty select_text warning");
            assert_eq!(status.tone, StatusTone::Warning);
            assert!(status.text.contains("Empty select"));
            assert!(status.text.contains("empty"));
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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

    #[gpui::test]
    fn filter_pane_keyboard_resize_sensitivity_scales_tap_and_hold_but_not_buttons(
        cx: &mut TestAppContext,
    ) {
        use resize::BASE_RESIZE_STEP;

        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("sample.log");
        std::fs::write(&file, "ERROR alpha\nINFO beta\nERROR gamma\n").unwrap();

        let measure = |cx: &mut TestAppContext, sensitivity: f32| -> (f32, f32, f32, f32) {
            let mut config = Config::load(None).unwrap();
            config.files.save_session = false;
            config.files.restore_session = false;
            config.window.filter_resize_sensitivity = sensitivity;
            cx.update(|cx| {
                cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
                cx.bind_keys(keybindings(&config).unwrap());
            });

            let (view, vcx) = cx.add_window_view(|window, cx| {
                Workspace::new(config, vec![file.clone()], window, cx)
            });
            wait_for_tab_loaded(&view, 0, vcx);

            let step_frame = |vcx: &mut VisualTestContext, ms: u64| {
                vcx.background_executor
                    .advance_clock(Duration::from_millis(ms));
                vcx.update(|window, cx| {
                    window.simulate_next_frame(cx);
                });
                vcx.run_until_parked();
            };
            let settle = |vcx: &mut VisualTestContext| {
                for _ in 0..45 {
                    step_frame(vcx, 16);
                }
            };

            let key_minus = Keystroke::parse("-").unwrap();
            let key_shift_minus = Keystroke::parse("shift--").unwrap();

            // Single tap `-` grows by BASE_RESIZE_STEP * sensitivity.
            vcx.simulate_keystrokes("-");
            vcx.simulate_event(KeyUpEvent {
                keystroke: key_minus.clone(),
            });
            settle(vcx);
            let after_grow_tap = view.read_with(vcx, |view, _| view.results_height);

            // Single tap `shift--` shrinks back by the same sensitivity-scaled step.
            vcx.simulate_keystrokes("shift--");
            vcx.simulate_event(KeyUpEvent {
                keystroke: key_shift_minus,
            });
            settle(vcx);
            let after_shrink_tap = view.read_with(vcx, |view, _| view.results_height);

            // Clicking the header grow button still uses the fixed BASE_RESIZE_STEP.
            let grow_btn = vcx
                .debug_bounds("results-grow")
                .expect("grow button")
                .center();
            vcx.simulate_click(grow_btn, Modifiers::none());
            vcx.run_until_parked();
            settle(vcx);
            let button_delta = view.read_with(vcx, |view, _| view.results_height - 250.0);

            // Reset height back to 100px to leave plenty of room for a long-press glide comparison.
            view.update(vcx, |view, cx| {
                view.resize_motion.cancel();
                view.results_height = 100.0;
                view.target_results_height = 100.0;
                cx.notify();
            });
            vcx.run_until_parked();

            // Sustained hold of `-` for ~450ms glides faster with higher sensitivity.
            vcx.simulate_event(KeyDownEvent {
                keystroke: key_minus.clone(),
                is_held: false,
                prefer_character_input: false,
            });
            for i in 0..28 {
                if i >= 14 && i % 3 == 0 {
                    vcx.simulate_event(KeyDownEvent {
                        keystroke: key_minus.clone(),
                        is_held: true,
                        prefer_character_input: false,
                    });
                }
                step_frame(vcx, 16);
            }
            vcx.simulate_event(KeyUpEvent {
                keystroke: key_minus,
            });
            settle(vcx);
            let hold_displacement = view.read_with(vcx, |view, _| view.results_height - 100.0);

            (
                after_grow_tap - 250.0,
                after_shrink_tap,
                button_delta,
                hold_displacement,
            )
        };

        let (tap_low, reset_low, btn_low, hold_low) = measure(cx, 0.5);
        let (tap_high, reset_high, btn_high, hold_high) = measure(cx, 2.5);

        assert_eq!(tap_low, BASE_RESIZE_STEP * 0.5);
        assert_eq!(reset_low, 250.0);
        assert_eq!(btn_low, BASE_RESIZE_STEP);

        assert_eq!(tap_high, BASE_RESIZE_STEP * 2.5);
        assert_eq!(reset_high, 250.0);
        assert_eq!(btn_high, BASE_RESIZE_STEP);

        assert!(
            hold_high > hold_low * 3.5,
            "2.5x sensitivity hold should glide much further than 0.5x (low={hold_low}, high={hold_high})"
        );
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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
            cx.bind_keys(text_input_default_bindings());
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

    #[gpui::test]
    fn left_and_right_arrows_scroll_active_pane_horizontally_without_affecting_inputs(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wide-keys.log");
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
            view.tabs[0].follow = true;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert_eq!(
                view.theme.shortcut(Command::ScrollLeft).as_deref(),
                Some("←")
            );
            assert_eq!(
                view.theme.shortcut(Command::ScrollRight).as_deref(),
                Some("→")
            );
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
            assert_eq!(
                f32::from(view.tabs[0].result_scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });

        // Pressing left at x=0 is a clean no-op.
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });

        // Scroll vertically first to verify horizontal scrolling preserves vertical position,
        // selected line, and follow mode.
        view.update(cx, |view, cx| {
            set_uniform_list_axis_offset(&view.tabs[0].scroll, ScrollAxis::Vertical, 120.0);
            cx.notify();
        });
        cx.run_until_parked();

        let main_step_100 = view.read_with(cx, |view, _| {
            let item = view.tabs[0]
                .scroll
                .0
                .borrow()
                .last_item_size
                .expect("main list laid out");
            horizontal_scroll_step(f32::from(item.item.width), 1.0)
        });

        // Pressing right in main pane scrolls only main pane horizontally and moves its scrollbar thumb.
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let offset = view.tabs[0].scroll.0.borrow().base_handle.offset();
            assert!((f32::from(offset.x) + main_step_100).abs() < 0.5);
            assert!((f32::from(offset.y) + 120.0).abs() < 0.5);
            assert_eq!(view.tabs[0].selected, 0);
            assert!(view.tabs[0].follow);
            assert_eq!(
                f32::from(view.tabs[0].result_scroll.0.borrow().base_handle.offset().x),
                0.0,
                "scrolling main pane must not move filter pane"
            );
            let hlayout = view
                .main_hscroll_state
                .layout()
                .expect("main horizontal scrollbar layout");
            assert!(hlayout.thumb.start > 0.0);
        });

        // Pressing left returns smoothly back to x=0.
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });

        // Repeated right presses clamp cleanly at max_offset without overshooting.
        let max_main_offset = view.read_with(cx, |view, _| {
            view.main_hscroll_state
                .layout()
                .expect("main horizontal scrollbar layout")
                .max_offset
        });
        let presses = (max_main_offset / main_step_100).ceil() as usize + 4;
        for _ in 0..presses {
            cx.simulate_keystrokes("right");
        }
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let offset_x = f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x);
            assert!(
                (offset_x + max_main_offset).abs() < 0.5,
                "expected clamp at -{max_main_offset}, got {offset_x}"
            );
        });

        // Switch to filter (results) pane with Tab: right/left scroll only the results pane.
        let main_offset_before_results = view.read_with(cx, |view, _| {
            f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x)
        });
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let results_x = f32::from(view.tabs[0].result_scroll.0.borrow().base_handle.offset().x);
            assert!(
                results_x < -10.0,
                "expected results pane to scroll right, got {results_x}"
            );
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                main_offset_before_results,
                "scrolling results pane must not move main pane"
            );
            let hlayout = view
                .results_hscroll_state
                .layout()
                .expect("results horizontal scrollbar layout");
            assert!(hlayout.thumb.start > 0.0);
        });
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                f32::from(view.tabs[0].result_scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });

        // Switch back to main pane, reset x=0, and verify horizontal step scales with zoom.
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            set_uniform_list_axis_offset(&view.tabs[0].scroll, ScrollAxis::Horizontal, 0.0);
            view.set_zoom_percent(200, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        let zoomed_offset_x = view.read_with(cx, |view, _| {
            -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x)
        });
        assert!(
            zoomed_offset_x > main_step_100 * 1.5,
            "expected 200% zoom step ({zoomed_offset_x}) to exceed 100% step ({main_step_100})"
        );

        // Inside Search, FilePath, and GoTo inputs, left/right edit the text cursor and never scroll logs.
        view.update_in(cx, |view, window, cx| {
            view.set_zoom_percent(DEFAULT_ZOOM_PERCENT, cx);
            set_uniform_list_axis_offset(&view.tabs[0].scroll, ScrollAxis::Horizontal, 0.0);
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("ab");
        cx.simulate_keystrokes("left");
        cx.simulate_input("X");
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "aXb");
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });

        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("right");
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });

        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("12");
        cx.simulate_keystrokes("left");
        cx.simulate_input("9");
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.goto.read(cx).as_str(), "192");
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });
    }

    #[gpui::test]
    fn left_and_right_arrows_are_noops_when_lines_fit_without_horizontal_overflow(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("short.log");
        std::fs::write(&path, "short line 0\nshort line 1\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            cx.notify();
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("right");
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.main_hscroll_state.layout().is_none());
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });
    }

    fn assert_surrounding_margin(
        handle: &UniformListScrollHandle,
        row: usize,
        row_count: usize,
        row_height: f32,
        surrounding_lines: usize,
        label: &str,
    ) {
        let state = handle.0.borrow();
        let item_size = state
            .last_item_size
            .expect("list should have been laid out");
        let viewport_h = f32::from(item_size.item.height);
        let scroll_top = -f32::from(state.base_handle.offset().y);
        let content_h = row_count as f32 * row_height;
        let max_scroll = (content_h - viewport_h).max(0.0);
        let desired_margin = surrounding_lines as f32 * row_height;
        let effective_margin = desired_margin.min(((viewport_h - row_height) * 0.5).max(0.0));
        let row_top = row as f32 * row_height;
        let row_bottom = row_top + row_height;
        let above = row_top - scroll_top;
        let below = (scroll_top + viewport_h) - row_bottom;

        if scroll_top > 0.5 {
            assert!(
                above + 0.5 >= effective_margin,
                "{label}: row {row} only has {above}px above (expected >= {effective_margin}px, scroll_top={scroll_top}, vp={viewport_h})"
            );
        }
        if scroll_top + 0.5 < max_scroll {
            assert!(
                below + 0.5 >= effective_margin,
                "{label}: row {row} only has {below}px below (expected >= {effective_margin}px, scroll_top={scroll_top}, vp={viewport_h}, max={max_scroll})"
            );
        }
    }

    #[gpui::test]
    fn cursor_surrounding_lines_keeps_gap_in_main_and_filter_panes_and_preserves_zero_default(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("surrounding.log");
        let content: String = (0..200)
            .map(|i| {
                let level = if i % 2 == 0 { "ERROR" } else { "INFO" };
                format!(
                    "2026-11-26 {level} line-{i:04} token_{i:04} {}",
                    "x".repeat(400)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        // 1. Default (0): stepping down through all initially visible rows does not scroll.
        let mut default_config = Config::load(None).unwrap();
        default_config.files.save_session = false;
        default_config.files.restore_session = false;
        assert_eq!(default_config.viewer.cursor_surrounding_lines, 0);
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&default_config).unwrap());
        });
        let (default_view, default_cx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::new(default_config.clone(), Vec::new(), window, cx);
            workspace.open(path.clone(), cx);
            workspace.tabs[0].log = Some(log.clone());
            workspace.tabs[0].loading = false;
            workspace
        });
        default_cx.run_until_parked();
        let initial_full_rows = default_view.read_with(default_cx, |view, _| {
            let vp = f32::from(
                view.tabs[0]
                    .scroll
                    .0
                    .borrow()
                    .last_item_size
                    .unwrap()
                    .item
                    .height,
            );
            (vp / view.zoom_metrics().line_height).floor() as usize
        });
        assert!(
            initial_full_rows >= 10,
            "expected at least 10 visible rows, got {initial_full_rows}"
        );
        default_view.update_in(default_cx, |view, window, cx| {
            for _ in 0..(initial_full_rows - 1) {
                view.command(Command::NextLine, window, cx);
            }
        });
        default_cx.run_until_parked();
        default_view.read_with(default_cx, |view, _| {
            assert_eq!(view.tabs[0].selected, initial_full_rows - 1);
            let scroll_top = -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(
                scroll_top.abs() < 0.5,
                "default cursor_surrounding_lines=0 should not scroll on last fully visible row, got {scroll_top}"
            );
        });

        // 2. Main pane with cursor_surrounding_lines = 4.
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.cursor_surrounding_lines = 4;
        let (view, vcx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::new(config.clone(), Vec::new(), window, cx);
            workspace.open(path.clone(), cx);
            workspace.tabs[0].log = Some(log.clone());
            workspace.tabs[0].loading = false;
            workspace
        });
        vcx.run_until_parked();

        // Preserve horizontal scroll while stepping down with surrounding lines.
        view.update(vcx, |view, cx| {
            set_uniform_list_axis_offset(&view.tabs[0].scroll, ScrollAxis::Horizontal, 120.0);
            cx.notify();
        });
        vcx.run_until_parked();

        let line_height = view.read_with(vcx, |view, _| view.zoom_metrics().line_height);
        let main_vp = view.read_with(vcx, |view, _| {
            f32::from(
                view.tabs[0]
                    .scroll
                    .0
                    .borrow()
                    .last_item_size
                    .unwrap()
                    .item
                    .height,
            )
        });
        let main_full_rows = (main_vp / line_height).floor() as usize;
        assert!(main_full_rows >= 10);

        // Stepping down starts scrolling 4 rows before the bottom edge and preserves horizontal scroll.
        let mut scrolled_early = false;
        for step in 1..=50 {
            view.update_in(vcx, |view, window, cx| {
                view.command(Command::NextLine, window, cx);
            });
            vcx.run_until_parked();
            view.read_with(vcx, |view, _| {
                assert_eq!(view.tabs[0].selected, step);
                assert_surrounding_margin(
                    &view.tabs[0].scroll,
                    step,
                    200,
                    line_height,
                    4,
                    "main NextLine",
                );
                let offset = view.tabs[0].scroll.0.borrow().base_handle.offset();
                assert!(
                    (f32::from(offset.x) - -120.0).abs() < 1.0,
                    "horizontal scroll must be preserved during vertical reveal, got {:?}",
                    offset.x
                );
                if step == main_full_rows - 2 && -f32::from(offset.y) > 0.5 {
                    scrolled_early = true;
                }
            });
        }
        assert!(
            scrolled_early,
            "expected main pane to start scrolling before reaching the bottom edge"
        );

        // Stepping back up stays stationary inside the safe band, then scrolls with 4 rows above.
        let scroll_before_up = view.read_with(vcx, |view, _| {
            -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y)
        });
        view.update_in(vcx, |view, window, cx| {
            view.command(Command::PreviousLine, window, cx);
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            let after_one_up = -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(
                (after_one_up - scroll_before_up).abs() < 0.5,
                "stepping up inside the safe band should not scroll (before={scroll_before_up}, after={after_one_up})"
            );
        });
        for _ in 0..30 {
            view.update_in(vcx, |view, window, cx| {
                view.command(Command::PreviousLine, window, cx);
            });
            vcx.run_until_parked();
            view.read_with(vcx, |view, _| {
                let row = view.tabs[0].selected;
                assert_surrounding_margin(
                    &view.tabs[0].scroll,
                    row,
                    200,
                    line_height,
                    4,
                    "main PreviousLine",
                );
            });
        }

        // Shift+Down range extension and Alt+O swap anchor keep the 4-row margin.
        view.update_in(vcx, |view, window, cx| {
            for _ in 0..25 {
                view.command(Command::SelectNextLine, window, cx);
            }
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            assert_surrounding_margin(
                &view.tabs[0].scroll,
                view.tabs[0].selected,
                200,
                line_height,
                4,
                "main SelectNextLine",
            );
        });
        view.update_in(vcx, |view, window, cx| {
            view.command(Command::SwapSelectionAnchor, window, cx);
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            assert_surrounding_margin(
                &view.tabs[0].scroll,
                view.tabs[0].selected,
                200,
                line_height,
                4,
                "main SwapSelectionAnchor",
            );
        });

        // LastLine clamps cleanly to bottom; FirstLine returns cleanly to top.
        view.update_in(vcx, |view, window, cx| {
            view.command(Command::LastLine, window, cx);
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            assert_eq!(view.tabs[0].selected, 199);
            assert_eq!(view.tabs[0].scroll.is_scrolled_to_end(), Some(true));
        });
        view.update_in(vcx, |view, window, cx| {
            view.command(Command::FirstLine, window, cx);
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            assert_eq!(view.tabs[0].selected, 0);
            let scroll_top = -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(scroll_top.abs() < 0.5);
        });

        // GoToLine / NextBookmark on a row inside the bottom 4-row edge margin scrolls to center it.
        let edge_row = main_full_rows - 1;
        view.update_in(vcx, |view, window, cx| {
            view.tabs[0].bookmarks.insert(edge_row);
            view.command(Command::NextBookmark, window, cx);
        });
        vcx.run_until_parked();
        view.read_with(vcx, |view, _| {
            assert_eq!(view.tabs[0].selected, edge_row);
            let scroll_top = -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(
                scroll_top > 0.5,
                "NextBookmark onto edge row {edge_row} must scroll to satisfy surrounding margin"
            );
            assert_surrounding_margin(
                &view.tabs[0].scroll,
                edge_row,
                200,
                line_height,
                4,
                "main NextBookmark",
            );
        });

        // Mouse clicking a visible row near the bottom edge does not yank it away mid-double-click.
        view.update_in(vcx, |view, window, cx| {
            view.command(Command::FirstLine, window, cx);
        });
        vcx.run_until_parked();
        let bottom_visible_row = main_full_rows - 2;
        let row0_bounds = vcx.debug_bounds("main-text-0").expect("row 0 text bounds");
        let gutter0_bounds = vcx
            .debug_bounds("main-gutter-0")
            .expect("row 0 gutter bounds");
        let scroll_before_click = view.read_with(vcx, |view, _| {
            -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y)
        });
        let click_pos = point(
            row0_bounds.left().max(gutter0_bounds.right()) + px(8.0),
            row0_bounds.center().y + px(bottom_visible_row as f32 * line_height),
        );
        for count in [1, 2] {
            vcx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position: click_pos,
                modifiers: Modifiers::none(),
                click_count: count,
                first_mouse: false,
            });
            vcx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position: click_pos,
                modifiers: Modifiers::none(),
                click_count: count,
            });
            vcx.run_until_parked();
        }
        view.read_with(vcx, |view, _| {
            assert_eq!(view.tabs[0].selected, bottom_visible_row);
            let scroll_after_click =
                -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(
                (scroll_after_click - scroll_before_click).abs() < 0.5,
                "clicking a visible row must not yank it out from under the pointer"
            );
            let sel = view.tabs[0]
                .main_text_selection
                .expect("double-click should select a word on the clicked row");
            assert_eq!(sel.row_range(), bottom_visible_row..=bottom_visible_row);
        });

        // 3. Filter/results pane also respects cursor_surrounding_lines when navigating filtered rows.
        view.update_in(vcx, |view, window, cx| {
            let matches: Vec<usize> = (0..200).filter(|i| i % 2 == 0).collect();
            view.tabs[0].results.matches = matches.clone();
            view.tabs[0].results.visible = matches;
            view.tabs[0].result_selection = LineSelection::single(0);
            view.focus_pane(Pane::Results, window, cx);
        });
        vcx.run_until_parked();
        for step in 1..=25 {
            view.update_in(vcx, |view, window, cx| {
                view.command(Command::NextLine, window, cx);
            });
            vcx.run_until_parked();
            view.read_with(vcx, |view, _| {
                assert_eq!(view.tabs[0].result_selection.cursor, step);
                assert_surrounding_margin(
                    &view.tabs[0].result_scroll,
                    step,
                    100,
                    line_height,
                    4,
                    "results NextLine",
                );
                assert_surrounding_margin(
                    &view.tabs[0].scroll,
                    step * 2,
                    200,
                    line_height,
                    4,
                    "main synced from results NextLine",
                );
            });
        }
        for _ in 0..15 {
            view.update_in(vcx, |view, window, cx| {
                view.command(Command::PreviousLine, window, cx);
            });
            vcx.run_until_parked();
            view.read_with(vcx, |view, _| {
                let row = view.tabs[0].result_selection.cursor;
                assert_surrounding_margin(
                    &view.tabs[0].result_scroll,
                    row,
                    100,
                    line_height,
                    4,
                    "results PreviousLine",
                );
            });
        }
    }

    #[gpui::test]
    fn cursor_surrounding_lines_adapts_to_zoom_centers_when_large_and_handles_initial_load(
        cx: &mut TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zoom_surrounding.log");
        let content: String = (0..200)
            .map(|i| format!("2026-11-26 INFO row-{i:04} payload\n"))
            .collect();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        // 1. Zooming scales the surrounding margin in rows (using zoomed line_height).
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.cursor_surrounding_lines = 4;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let (view, vcx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::new(config.clone(), Vec::new(), window, cx);
            workspace.open(path.clone(), cx);
            workspace.tabs[0].log = Some(log.clone());
            workspace.tabs[0].loading = false;
            workspace
        });
        vcx.run_until_parked();

        view.update_in(vcx, |view, window, cx| {
            for _ in 0..5 {
                view.command(Command::ZoomIn, window, cx);
            }
        });
        vcx.run_until_parked();
        let zoomed_line_height = view.read_with(vcx, |view, _| {
            assert_eq!(view.zoom_percent, 150);
            view.zoom_metrics().line_height
        });
        assert!((zoomed_line_height - 36.0).abs() < 0.5);

        for _ in 0..25 {
            view.update_in(vcx, |view, window, cx| {
                view.command(Command::NextLine, window, cx);
            });
            vcx.run_until_parked();
            view.read_with(vcx, |view, _| {
                assert_surrounding_margin(
                    &view.tabs[0].scroll,
                    view.tabs[0].selected,
                    200,
                    zoomed_line_height,
                    4,
                    "zoomed NextLine",
                );
            });
        }

        // 2. Large cursor_surrounding_lines (larger than half viewport) centers cursor vertically.
        let mut center_config = Config::load(None).unwrap();
        center_config.files.save_session = false;
        center_config.files.restore_session = false;
        center_config.viewer.cursor_surrounding_lines = 500;
        let (center_view, center_cx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::new(center_config.clone(), Vec::new(), window, cx);
            workspace.open(path.clone(), cx);
            workspace.tabs[0].log = Some(log.clone());
            workspace.tabs[0].loading = false;
            workspace
        });
        center_cx.run_until_parked();

        center_view.update_in(center_cx, |view, window, cx| {
            for _ in 0..50 {
                view.command(Command::NextLine, window, cx);
            }
        });
        center_cx.run_until_parked();
        center_view.read_with(center_cx, |view, _| {
            let state = view.tabs[0].scroll.0.borrow();
            let vp = f32::from(state.last_item_size.unwrap().item.height);
            let line_h = view.zoom_metrics().line_height;
            let scroll_top = -f32::from(state.base_handle.offset().y);
            let expected_center = (50.0 * line_h + line_h * 0.5) - vp * 0.5;
            assert!(
                (scroll_top - expected_center).abs() < 1.0,
                "expected cursor centered at {expected_center}, got {scroll_top}"
            );
        });

        // 3. Initial load before first layout resolves pending reveal once viewport size is known,
        //    even when the restored line sits inside the bottom margin of the first screen.
        let initial_edge_row = center_view.read_with(center_cx, |view, _| {
            let vp = f32::from(
                view.tabs[0]
                    .scroll
                    .0
                    .borrow()
                    .last_item_size
                    .unwrap()
                    .item
                    .height,
            );
            ((vp / view.zoom_metrics().line_height).floor() as usize).saturating_sub(1)
        });
        assert!(initial_edge_row >= 8);
        let mut init_config = Config::load(None).unwrap();
        init_config.files.save_session = false;
        init_config.files.restore_session = false;
        init_config.viewer.cursor_surrounding_lines = 5;
        let (init_view, init_cx) = cx.add_window_view(|window, cx| {
            let mut workspace = Workspace::new(init_config.clone(), Vec::new(), window, cx);
            workspace.open(path.clone(), cx);
            workspace.tabs[0].selected = initial_edge_row;
            workspace.tabs[0].main_selection = LineSelection::single(initial_edge_row);
            workspace.tabs[0].log = Some(log.clone());
            workspace.tabs[0].loading = false;
            let lh = workspace.zoom_metrics().line_height;
            Workspace::reveal_main_row_in_tab(
                &mut workspace.tabs[0],
                initial_edge_row,
                lh,
                5,
                RevealStrategy::Center,
            );
            workspace
        });
        init_cx.run_until_parked();
        // Advance the scheduled next-frame callback (tests have no platform frame loop).
        init_cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        init_cx.run_until_parked();
        init_view.read_with(init_cx, |view, _| {
            assert!(
                view.tabs[0].pending_main_reveal.is_none(),
                "pending reveal should have been flushed after initial layout"
            );
            let scroll_top = -f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().y);
            assert!(
                scroll_top > 0.5,
                "deferred initial reveal must scroll edge row {initial_edge_row} into the safe margin"
            );
            assert_surrounding_margin(
                &view.tabs[0].scroll,
                initial_edge_row,
                200,
                view.zoom_metrics().line_height,
                5,
                "initial load deferred reveal",
            );
        });
    }

    #[gpui::test]
    fn text_navigation_line_start_and_end_work_across_all_text_inputs(cx: &mut TestAppContext) {
        assert_eq!(
            config::KeyContext::TextInput.predicate(),
            DEFAULT_INPUT_CONTEXT
        );

        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nav.log");
        std::fs::write(&path, "line 0\nline 1\nline 2\nline 3\nline 4\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].selected = 1;
            view.tabs[0].main_selection = LineSelection::single(1);
            cx.notify();
        });
        cx.run_until_parked();

        let caret_range = |input: &Entity<EditableTextState>,
                           window: &mut Window,
                           cx: &mut Context<Workspace>| {
            input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range
        };

        // 1. Search box (`Search` / `TextInput`): cmd-left/right and home/end move caret,
        //    clear active selection, and handle Unicode text.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("alpha beta");
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });
        cx.simulate_input("[");
        cx.simulate_keystrokes("cmd-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let end = view.input.read(cx).as_str().encode_utf16().count();
            assert_eq!(caret_range(&view.input, window, cx), end..end);
        });
        cx.simulate_input("]");
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });
        cx.simulate_input("<");
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let end = view.input.read(cx).as_str().encode_utf16().count();
            assert_eq!(caret_range(&view.input, window, cx), end..end);
        });
        cx.simulate_input(">");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "<[alpha beta]>");
        });

        // Collapses active selection cleanly and handles multibyte Unicode text.
        let select_all = if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        };
        cx.simulate_keystrokes(select_all);
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
            view.input
                .update(cx, |state, cx| state.emplace("שלום мир", cx));
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let end = "שלום мир".encode_utf16().count();
            assert_eq!(caret_range(&view.input, window, cx), end..end);
        });

        // 2. File path editing (`FilePath` / `TextInput`): collapses the initial full selection
        //    and moves to start/end with home/end and cmd-left/cmd-right without opening completions.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        let initial_path =
            view.read_with(cx, |view, cx| view.path_input.read(cx).as_str().to_owned());
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.path_input, window, cx), 0..0);
            assert_eq!(view.path_input.read(cx).as_str(), initial_path);
            assert!(!view.show_path_completions);
        });
        cx.simulate_keystrokes("cmd-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let end = initial_path.encode_utf16().count();
            assert_eq!(caret_range(&view.path_input, window, cx), end..end);
        });
        cx.simulate_input(".bak");
        cx.simulate_keystrokes("cmd-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.path_input, window, cx), 0..0);
        });
        cx.simulate_input("!");
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let expected = format!("!{initial_path}.bak");
            assert_eq!(view.path_input.read(cx).as_str(), expected);
            let end = expected.encode_utf16().count();
            assert_eq!(caret_range(&view.path_input, window, cx), end..end);
            assert_eq!(view.tabs[0].selected, 1);
        });

        // 3. Pattern search box (`Presets` / `TextInput`): cmd-left/right and home/end navigate
        //    the pattern search text instead of jumping preset_index.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::TogglePresets, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 1));
        cx.simulate_input("err");
        cx.run_until_parked();
        let preset_index_after_filter = view.read_with(cx, |view, _| view.preset_index);
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.presets_search, window, cx), 0..0);
            assert_eq!(view.preset_index, preset_index_after_filter);
        });
        cx.simulate_input("^");
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let end = view.presets_search.read(cx).as_str().encode_utf16().count();
            assert_eq!(caret_range(&view.presets_search, window, cx), end..end);
            assert_eq!(view.preset_index, preset_index_after_filter);
        });
        cx.simulate_input("$");
        cx.simulate_keystrokes("cmd-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.presets_search, window, cx), 0..0);
            assert_eq!(view.presets_search.read(cx).as_str(), "^err$");
        });
        cx.simulate_keystrokes("cmd-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.presets_search, window, cx), 5..5);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 4. Go-to-line input (`GoTo` / `TextInput`): home/end and cmd-left/right move caret.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("42");
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.goto, window, cx), 0..0);
        });
        cx.simulate_input("1");
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.goto, window, cx), 3..3);
        });
        cx.simulate_input("0");
        cx.simulate_keystrokes("cmd-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.goto, window, cx), 0..0);
            assert_eq!(view.goto.read(cx).as_str(), "1420");
        });
        cx.simulate_keystrokes("cmd-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.goto, window, cx), 4..4);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 5. Log pane (`Log` context) retains home/end line navigation untouched.
        view.update_in(cx, |view, window, cx| {
            assert!(view.focus.is_focused(window));
            assert_eq!(view.tabs[0].selected, 1);
            // Calling text navigation commands when no text input is focused is a safe no-op.
            view.command(Command::MoveToLineStart, window, cx);
            view.command(Command::MoveToLineEnd, window, cx);
            assert_eq!(view.tabs[0].selected, 1);
        });
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.tabs[0].selected, 4));
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.tabs[0].selected, 0));
    }

    #[gpui::test]
    fn text_navigation_supports_custom_bindings_and_context_specific_overrides(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        // Add extra user keybindings for line start/end across all text inputs.
        config.keyboard.bindings.push(config::Binding {
            keys: "ctrl-a".into(),
            context: config::KeyContext::TextInput,
            action: Command::MoveToLineStart,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "ctrl-e".into(),
            context: config::KeyContext::TextInput,
            action: Command::MoveToLineEnd,
        });
        // Intentionally register a specific `Presets` override BEFORE a `TextInput` binding
        // for the same key to verify specificity ordering is independent of JSON order.
        config.keyboard.bindings.push(config::Binding {
            keys: "alt-home".into(),
            context: config::KeyContext::Presets,
            action: Command::FirstLine,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "alt-home".into(),
            context: config::KeyContext::TextInput,
            action: Command::MoveToLineStart,
        });
        config.validate().unwrap();
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-nav.log");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            cx.notify();
        });
        cx.run_until_parked();

        let caret_range = |input: &Entity<EditableTextState>,
                           window: &mut Window,
                           cx: &mut Context<Workspace>| {
            input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range
        };

        // Custom ctrl-a / ctrl-e and alt-home work in Search.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("hello");
        cx.simulate_keystrokes("ctrl-a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });
        cx.simulate_keystrokes("ctrl-e");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.input, window, cx), 5..5);
        });
        cx.simulate_keystrokes("alt-home");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });

        // Custom ctrl-a / ctrl-e work in FilePath.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.path_input, window, cx), 0..0);
        });
        cx.simulate_keystrokes("ctrl-e");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let end = view.path_input.read(cx).as_str().encode_utf16().count();
            assert_eq!(caret_range(&view.path_input, window, cx), end..end);
        });

        // Custom ctrl-a / ctrl-e work in GoTo.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("99");
        cx.simulate_keystrokes("ctrl-a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.goto, window, cx), 0..0);
        });
        cx.simulate_keystrokes("ctrl-e");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.goto, window, cx), 2..2);
        });

        // In Presets, custom ctrl-a / ctrl-e move text caret, while Presets-specific alt-home
        // overrides TextInput's alt-home regardless of binding order.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::TogglePresets, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 2));
        cx.simulate_input("e");
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 1));
        cx.simulate_keystrokes("ctrl-a");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.presets_search, window, cx), 0..0);
            assert_eq!(view.preset_index, 1);
        });
        cx.simulate_keystrokes("ctrl-e");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(caret_range(&view.presets_search, window, cx), 1..1);
        });
        cx.simulate_keystrokes("alt-home");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.preset_index, 0);
            assert_eq!(caret_range(&view.presets_search, window, cx), 1..1);
        });

        // Verify shortcut label formatting for cmd/home/end tokens.
        assert_eq!(theme::format_shortcut("home"), "Home");
        assert_eq!(theme::format_shortcut("end"), "End");
        if cfg!(target_os = "macos") {
            assert_eq!(theme::format_shortcut("cmd-left"), "⌘←");
            assert_eq!(theme::format_shortcut("cmd-right"), "⌘→");
        } else if cfg!(target_os = "windows") {
            assert_eq!(theme::format_shortcut("cmd-left"), "Win+←");
            assert_eq!(theme::format_shortcut("cmd-right"), "Win+→");
        } else {
            assert_eq!(theme::format_shortcut("cmd-left"), "Super+←");
            assert_eq!(theme::format_shortcut("cmd-right"), "Super+→");
        }
    }

    #[gpui::test]
    fn quit_shortcut_works_when_no_logs_are_loaded(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let quit_key = if cfg!(target_os = "macos") {
            "cmd-q"
        } else {
            "ctrl-q"
        };
        let focus_search_key = if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-f"
        };
        let focus_path_key = if cfg!(target_os = "macos") {
            "cmd-shift-o"
        } else {
            "ctrl-shift-o"
        };
        let goto_key = if cfg!(target_os = "macos") {
            "cmd-l"
        } else {
            "ctrl-l"
        };
        let presets_key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, Vec::new(), window, cx));
        cx.run_until_parked();

        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, 0);
        });

        // Pressing Cmd+Q / Ctrl+Q immediately on the empty welcome screen triggers Quit.
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.quit_count, 1));

        // Interacting with the welcome screen, header, and file library preserves Workspace focus.
        let welcome_pt =
            cx.debug_bounds("welcome").expect("welcome bounds").origin + point(px(16.), px(16.));
        cx.simulate_click(welcome_pt, Modifiers::none());
        cx.run_until_parked();

        let library_btn = cx
            .debug_bounds("header-library")
            .expect("library button bounds")
            .center();
        cx.simulate_click(library_btn, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.show_files));
        cx.simulate_click(library_btn, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(!view.show_files));

        // Tab-only Workspace shortcuts (search, file path, go to line, presets) do not focus
        // unmounted inputs or break Quit on the welcome screen.
        for key in [focus_search_key, focus_path_key, goto_key, presets_key] {
            cx.simulate_keystrokes(key);
            cx.run_until_parked();
        }
        view.update_in(cx, |view, window, cx| {
            assert!(view.focus.is_focused(window));
            assert!(!view.input.read(cx).focus_handle(cx).is_focused(window));
            assert!(!view.path_input.read(cx).focus_handle(cx).is_focused(window));
            assert!(!view.goto.read(cx).focus_handle(cx).is_focused(window));
            assert!(!view.show_goto);
            assert!(!view.show_presets);
        });

        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.quit_count, 2));

        // Log-only shortcuts are inactive on the welcome screen and do not disrupt Quit.
        cx.simulate_keystrokes("m f down");
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.quit_count, 3));

        // Even if the window loses focus, on_focus_lost restores welcome focus so Quit still works.
        view.update_in(cx, |_view, window, cx| window.blur(cx));
        cx.run_until_parked();
        view.update_in(cx, |view, window, _cx| {
            assert!(view.focus.is_focused(window));
        });
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.quit_count, 4));
    }

    #[gpui::test]
    fn quit_shortcut_works_after_closing_last_tab_from_any_focused_control(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let quit_key = if cfg!(target_os = "macos") {
            "cmd-q"
        } else {
            "ctrl-q"
        };
        let close_tab_key = if cfg!(target_os = "macos") {
            "cmd-w"
        } else {
            "ctrl-w"
        };

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.log");
        std::fs::write(&path, "ERROR first\nINFO second\nERROR third\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, Vec::new(), window, cx));
        cx.run_until_parked();

        let open_loaded_tab = |view: &Entity<Workspace>, cx: &mut VisualTestContext| {
            let log = log.clone();
            let path = path.clone();
            view.update(cx, |view, cx| {
                view.open(path, cx);
                view.tabs[0].log = Some(log);
                view.tabs[0].loading = false;
                view.input
                    .update(cx, |input, cx| input.emplace("ERROR", cx));
                view.start_search(0, cx);
                cx.notify();
            });
            cx.run_until_parked();
        };

        let mut expected_quits = 0;

        // 1. Main log pane focused -> Cmd+W -> Cmd+Q.
        open_loaded_tab(&view, cx);
        cx.simulate_keystrokes(close_tab_key);
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        expected_quits += 1;
        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, expected_quits);
        });

        // 2. Search input focused -> Cmd+W -> Cmd+Q.
        open_loaded_tab(&view, cx);
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(close_tab_key);
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        expected_quits += 1;
        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, expected_quits);
        });

        // 3. Results pane focused -> click tab close button -> Cmd+Q.
        open_loaded_tab(&view, cx);
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Results, window, cx);
        });
        cx.run_until_parked();
        let close_btn = cx
            .debug_bounds("tab-close-0")
            .expect("tab close button bounds")
            .center();
        cx.simulate_click(close_btn, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        expected_quits += 1;
        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, expected_quits);
        });

        // 4. File path input focused -> Cmd+W -> Cmd+Q.
        open_loaded_tab(&view, cx);
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(close_tab_key);
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        expected_quits += 1;
        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, expected_quits);
        });

        // 5. Go-to-line input focused -> close_tab_at without window handle -> on_focus_lost -> Cmd+Q.
        open_loaded_tab(&view, cx);
        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| view.close_tab_at(0, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        expected_quits += 1;
        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, expected_quits);
        });

        // 6. Presets search input focused -> Cmd+W -> Cmd+Q.
        open_loaded_tab(&view, cx);
        view.update_in(cx, |view, window, cx| {
            view.command(Command::TogglePresets, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(close_tab_key);
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        expected_quits += 1;
        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, expected_quits);
        });

        // 7. Filter pane maximized (main pane hidden, results pane focused) -> Cmd+W -> Cmd+Q.
        open_loaded_tab(&view, cx);
        view.update_in(cx, |view, window, cx| {
            let (_min_h, max_h) = view.results_height_bounds(window);
            view.results_height = max_h;
            view.target_results_height = max_h;
            view.focus_pane(Pane::Results, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(close_tab_key);
        cx.run_until_parked();
        cx.simulate_keystrokes(quit_key);
        cx.run_until_parked();
        expected_quits += 1;
        view.update_in(cx, |view, window, _cx| {
            assert!(view.tabs.is_empty());
            assert!(view.focus.is_focused(window));
            assert_eq!(view.quit_count, expected_quits);
        });
    }

    #[gpui::test]
    fn copy_match_results_button_and_shortcut_copy_selected_lines_in_both_panes(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.search.context_lines = 1;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("copy_matches.log");
        std::fs::write(
            &path,
            concat!(
                "INFO boot\n",
                "WARN user=alice status=200 extra=ok\n",
                "DEBUG middle\n",
                "ERROR user=bob status=503 status=504\n",
                "INFO tail\n",
            ),
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, Vec::new(), window, cx));
        cx.run_until_parked();

        // Both copy buttons are hidden on the welcome screen, and default shortcut formats as "C".
        assert!(cx.debug_bounds("copy").is_none());
        assert!(cx.debug_bounds("copy-matches").is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.theme.shortcut(Command::CopyMatchResults).as_deref(),
                Some("C")
            );
        });

        // Open a tab and attach indexed log.
        view.update(cx, |view, cx| {
            view.open(path.clone(), cx);
            view.tabs[0].log = Some(log);
            view.tabs[0].loading = false;
            view.tabs[0].bookmarks.insert(0);
            view.refresh_bookmarks(0, cx);
            cx.notify();
        });
        cx.run_until_parked();

        let copy_bounds = cx.debug_bounds("copy").expect("copy button bounds");
        let copy_matches_bounds = cx
            .debug_bounds("copy-matches")
            .expect("copy-matches button bounds");
        let zoom_out_bounds = cx.debug_bounds("zoom-out").expect("zoom-out button bounds");
        let search_apply_bounds = cx
            .debug_bounds("search-apply")
            .expect("search-apply button bounds");
        assert_eq!(
            copy_bounds.top(),
            copy_matches_bounds.top(),
            "copy-matches button should sit in the header toolbar next to copy"
        );
        assert!(
            copy_bounds.right() < copy_matches_bounds.left()
                && copy_matches_bounds.right() < zoom_out_bounds.left(),
            "copy-matches button should sit immediately to the right of copy before zoom controls"
        );
        assert!(
            copy_matches_bounds.bottom() < search_apply_bounds.top(),
            "copy-matches button should sit in the header toolbar above the filter bar"
        );

        // Pressing `c` in the Main pane (Log context) with no search matches warns without copying.
        cx.simulate_keystrokes("c");
        assert!(cx.read_from_clipboard().is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::warning("No match results to copy"))
            );
        });

        // Focusing the Filter pane and pressing `c` with no matches also warns without copying.
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Results, window, cx);
            view.status = None;
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("c");
        assert!(cx.read_from_clipboard().is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::warning("No match results to copy"))
            );
        });

        // Typing `c` in the search input inserts text normally instead of triggering Log.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.simulate_input("case");
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "case");
        });
        assert!(cx.read_from_clipboard().is_none());

        // Search in Regex mode with capturing groups and context/bookmarks present.
        view.update_in(cx, |view, window, cx| {
            view.input.update(cx, |input, cx| {
                input.emplace(r"user=(\w+).*?status=(\d{3})", cx);
            });
            view.command(Command::Search, window, cx);
            view.focus_pane(Pane::Main, window, cx);
            view.status = None;
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].results.matches, vec![1, 3]);
            assert_eq!(view.tabs[0].results.visible, vec![0, 1, 2, 3, 4]);
        });

        // In Main pane with only non-matching line 0 selected: warns and copies nothing.
        cx.simulate_keystrokes("c");
        assert!(cx.read_from_clipboard().is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::warning("No match results in selected lines"))
            );
        });

        // In Main pane with matching line 1 selected: pressing `c` copies only that line's captures.
        cx.simulate_keystrokes("down c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("alice 200".to_owned())
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::success("Copied 1 match result"))
            );
        });

        // In Main pane with lines 1..=3 selected (includes non-matching line 2): copies lines 1 & 3.
        cx.simulate_keystrokes("shift-down shift-down c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("alice 200\nbob 503".to_owned())
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::success("Copied 2 match results"))
            );
        });

        // Empty selection in Main pane warns "No lines selected" and keeps clipboard intact.
        view.update(cx, |view, cx| {
            view.tabs[0].main_selection = LineSelection::empty();
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("alice 200\nbob 503".to_owned())
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::warning("No lines selected"))
            );
        });

        // Turning off highlight_matches in Regex mode still copies what would be highlighted
        // when clicking the copy-matches button for the active pane's selected lines.
        let select_all_key = if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        };
        view.update_in(cx, |view, window, cx| {
            view.command(Command::ToggleHighlightMatches, window, cx);
            assert!(!view.tabs[0].options.highlight_matches);
            view.tabs[0].main_selection.select_single(3);
            view.tabs[0].selected = 3;
            cx.notify();
        });
        cx.update(|_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("sentinel".to_owned()))
        });
        let copy_matches_bounds = cx
            .debug_bounds("copy-matches")
            .expect("copy-matches button bounds after search");
        cx.simulate_click(copy_matches_bounds.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("bob 503".to_owned())
        );

        // In Filter pane: row 0 is context/bookmark line 0 (not a match) -> warns, does not copy.
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Results, window, cx);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("bob 503".to_owned())
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::warning("No match results in selected lines"))
            );
        });

        // In Filter pane: moving down to row 1 (match line 1) copies only row 1, and `m` still works.
        cx.simulate_keystrokes("down c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("alice 200".to_owned())
        );
        cx.simulate_keystrokes("m");
        view.read_with(cx, |view, _| {
            assert!(
                view.tabs[0].bookmarks.contains(&1),
                "Log context shortcuts must remain active in the Filter pane"
            );
        });
        cx.simulate_keystrokes("m");

        // In Filter pane: Select All copies all matches while skipping context & bookmark rows.
        cx.simulate_keystrokes(select_all_key);
        cx.simulate_keystrokes("c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("alice 200\nbob 503".to_owned())
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::success("Copied 2 match results"))
            );
        });

        // Regex without capturing groups copies full regex matches, space-separated per line,
        // for only the selected rows in the Filter pane.
        view.update_in(cx, |view, window, cx| {
            view.input
                .update(cx, |input, cx| input.emplace(r"status=\d{3}", cx));
            view.command(Command::Search, window, cx);
            view.focus_pane(Pane::Results, window, cx);
        });
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.tabs[0].result_selection = LineSelection::single(3); // visible[3] = line 3
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("status=503 status=504".to_owned())
        );

        // Switching to Literal mode copies full matched lines from the selected rows only
        // (excluding context and bookmark rows even when selected via Select All).
        view.update_in(cx, |view, window, cx| {
            view.input
                .update(cx, |input, cx| input.emplace("status=", cx));
            view.command(Command::ToggleRegex, window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
            assert_eq!(view.tabs[0].results.matches, vec![1, 3]);
            assert_eq!(view.tabs[0].results.visible, vec![0, 1, 2, 3, 4]);
        });
        // Select rows 0..=2 (bookmark/context 0, match 1, context 2) -> only line 1 is copied.
        view.update(cx, |view, cx| {
            view.tabs[0].result_selection = LineSelection::single(0);
            view.tabs[0].result_selection.extend_to(2);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("WARN user=alice status=200 extra=ok".to_owned())
        );

        // Select All in Literal mode copies both matched lines without context/bookmarks.
        cx.simulate_keystrokes(select_all_key);
        cx.simulate_keystrokes("c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(
                "WARN user=alice status=200 extra=ok\nERROR user=bob status=503 status=504"
                    .to_owned()
            )
        );

        // Regex mode with a non-participating optional capture group on a selected matching line
        // warns and keeps clipboard intact.
        view.update_in(cx, |view, window, cx| {
            view.input
                .update(cx, |input, cx| input.emplace(r"INFO (missing)?", cx));
            view.command(Command::ToggleRegex, window, cx);
        });
        cx.run_until_parked();
        // Line 0 ("INFO boot") matches `INFO (missing)?` and is row 0 in visible results.
        cx.simulate_keystrokes("c");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(
                "WARN user=alice status=200 extra=ok\nERROR user=bob status=503 status=504"
                    .to_owned()
            )
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.status,
                Some(StatusMessage::warning("No highlighted match text to copy"))
            );
        });
    }

    #[gpui::test]
    fn filter_log_context_overrides_log_in_filter_pane_regardless_of_binding_order(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        // Put FilterLog BEFORE Log for the same key to verify specificity + DOM depth precedence.
        config.keyboard.bindings = vec![
            config::Binding {
                keys: "x".to_owned(),
                context: config::KeyContext::FilterLog,
                action: Command::CopyMatchResults,
            },
            config::Binding {
                keys: "x".to_owned(),
                context: config::KeyContext::Log,
                action: Command::ToggleBookmark,
            },
        ];
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("filter_context.log");
        std::fs::write(&path, "INFO alpha\nERROR beta\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update_in(cx, |view, window, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].loading = false;
            view.input
                .update(cx, |input, cx| input.emplace("ERROR (\\w+)", cx));
            view.command(Command::Search, window, cx);
            view.focus_pane(Pane::Main, window, cx);
        });
        cx.run_until_parked();

        // In Main pane, `x` runs the `Log` binding (ToggleBookmark), not `FilterLog`.
        cx.simulate_keystrokes("x");
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].bookmarks.contains(&0));
        });
        assert!(cx.read_from_clipboard().is_none());

        // In Filter pane, `x` runs the more specific `FilterLog` binding (CopyMatchResults).
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Results, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("x");
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("beta".to_owned())
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0]));
        });
    }

    #[gpui::test]
    fn text_selection_to_line_start_and_end_work_across_all_text_inputs(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("select-line.log");
        std::fs::write(&path, "line 0\nline 1\nline 2\nline 3\nline 4\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].selected = 1;
            view.tabs[0].main_selection = LineSelection::single(1);
            cx.notify();
        });
        cx.run_until_parked();

        let selection_state = |input: &Entity<EditableTextState>,
                               window: &mut Window,
                               cx: &mut Context<Workspace>| {
            input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
        };

        // 1. Search box (`Search` / `TextInput`): cmd-shift-left/right and shift-up/down select
        //    to line start/end, preserve anchor when reversing, replace selection on input, and
        //    handle boundary no-ops and multibyte/surrogate-pair Unicode text.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("alpha beta");
        cx.simulate_keystrokes("left left left left left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 5..5);
        });

        cx.simulate_keystrokes("cmd-shift-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 0..5);
            assert!(sel.reversed);
        });

        cx.simulate_keystrokes("cmd-shift-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 5..10);
            assert!(!sel.reversed);
        });
        cx.simulate_input(" gamma");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "alpha gamma");
        });

        cx.simulate_keystrokes("left left left left left left");
        cx.simulate_keystrokes("shift-up");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 0..5);
            assert!(sel.reversed);
        });

        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 5..11);
            assert!(!sel.reversed);
        });
        cx.simulate_input("!");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "alpha!");
        });

        // Boundary behavior: at start/end, selecting further is a safe no-op, and selecting the
        // opposite direction selects the entire line.
        cx.simulate_keystrokes("home cmd-shift-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(selection_state(&view.input, window, cx).range, 0..0);
        });
        cx.simulate_keystrokes("cmd-shift-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 0..6);
            assert!(!sel.reversed);
        });
        cx.simulate_keystrokes("end shift-down");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(selection_state(&view.input, window, cx).range, 6..6);
        });
        cx.simulate_keystrokes("shift-up");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 0..6);
            assert!(sel.reversed);
        });

        // Multibyte + UTF-16 surrogate pair Unicode text.
        let unicode = "שלום 🚀 мир";
        let split_utf8 = "שלום ".len();
        let split_utf16 = "שלום ".encode_utf16().count();
        let end_utf16 = unicode.encode_utf16().count();
        view.update_in(cx, |view, _window, cx| {
            view.input.update(cx, |state, cx| {
                state.emplace(unicode, cx);
                state.move_to(split_utf8, cx);
            });
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, 0..split_utf16);
            assert!(sel.reversed);
        });
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.input, window, cx);
            assert_eq!(sel.range, split_utf16..end_utf16);
            assert!(!sel.reversed);
        });

        // 2. File path bar (`FilePath` / `TextInput`): selects from caret to start/end without
        //    opening completions.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        let initial_path =
            view.read_with(cx, |view, cx| view.path_input.read(cx).as_str().to_owned());
        let path_end_utf16 = initial_path.encode_utf16().count();
        cx.simulate_keystrokes("home right right cmd-shift-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.path_input, window, cx);
            assert_eq!(sel.range, 2..path_end_utf16);
            assert!(!sel.reversed);
            assert!(!view.show_path_completions);
        });
        cx.simulate_keystrokes("shift-up");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.path_input, window, cx);
            assert_eq!(sel.range, 0..2);
            assert!(sel.reversed);
            assert!(!view.show_path_completions);
        });
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.path_input, window, cx);
            assert_eq!(sel.range, 2..path_end_utf16);
            assert!(!sel.reversed);
        });
        cx.simulate_keystrokes("cmd-shift-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.path_input, window, cx);
            assert_eq!(sel.range, 0..2);
            assert!(sel.reversed);
        });

        // 3. Pattern search box (`Presets` / `TextInput`): selects inside the filter text without
        //    moving `preset_index`.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::TogglePresets, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 1));
        cx.simulate_input("error");
        cx.run_until_parked();
        let preset_index_after_filter = view.read_with(cx, |view, _| view.preset_index);
        cx.simulate_keystrokes("left left shift-up");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.presets_search, window, cx);
            assert_eq!(sel.range, 0..3);
            assert!(sel.reversed);
            assert_eq!(view.preset_index, preset_index_after_filter);
        });
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.presets_search, window, cx);
            assert_eq!(sel.range, 3..5);
            assert!(!sel.reversed);
            assert_eq!(view.preset_index, preset_index_after_filter);
        });
        cx.simulate_keystrokes("cmd-shift-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.presets_search, window, cx);
            assert_eq!(sel.range, 0..3);
            assert!(sel.reversed);
        });
        cx.simulate_keystrokes("cmd-shift-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.presets_search, window, cx);
            assert_eq!(sel.range, 3..5);
            assert!(!sel.reversed);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 4. Go-to-line input (`GoTo` / `TextInput`).
        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("12345");
        cx.simulate_keystrokes("left left cmd-shift-left");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.goto, window, cx);
            assert_eq!(sel.range, 0..3);
            assert!(sel.reversed);
        });
        cx.simulate_keystrokes("cmd-shift-right");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.goto, window, cx);
            assert_eq!(sel.range, 3..5);
            assert!(!sel.reversed);
        });
        cx.simulate_keystrokes("shift-up");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.goto, window, cx);
            assert_eq!(sel.range, 0..3);
            assert!(sel.reversed);
        });
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let sel = selection_state(&view.goto, window, cx);
            assert_eq!(sel.range, 3..5);
            assert!(!sel.reversed);
        });
        cx.simulate_input("99");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.goto.read(cx).as_str(), "12399");
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 5. Log pane (`Log` context) retains shift-up / shift-down multi-line selection untouched,
        //    and invoking select-to-line commands with no focused text input is a safe no-op.
        view.update_in(cx, |view, window, cx| {
            assert!(view.focus.is_focused(window));
            assert_eq!(view.tabs[0].selected, 1);
            view.command(Command::SelectToLineStart, window, cx);
            view.command(Command::SelectToLineEnd, window, cx);
            assert_eq!(view.tabs[0].selected, 1);
            assert_eq!(view.tabs[0].main_selection.count(), 1);
        });
        cx.simulate_keystrokes("shift-down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 2);
            assert_eq!(view.tabs[0].main_selection.count(), 2);
            assert!(view.tabs[0].main_selection.contains(1));
            assert!(view.tabs[0].main_selection.contains(2));
        });
        cx.simulate_keystrokes("shift-up shift-up");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 0);
            assert_eq!(view.tabs[0].main_selection.count(), 2);
            assert!(view.tabs[0].main_selection.contains(0));
            assert!(view.tabs[0].main_selection.contains(1));
        });

        // Verify shortcut label formatting for the new default selection shortcuts.
        if cfg!(target_os = "macos") {
            assert_eq!(theme::format_shortcut("cmd-shift-left"), "⌘⇧←");
            assert_eq!(theme::format_shortcut("cmd-shift-right"), "⌘⇧→");
            assert_eq!(theme::format_shortcut("shift-up"), "⇧↑");
            assert_eq!(theme::format_shortcut("shift-down"), "⇧↓");
        } else if cfg!(target_os = "windows") {
            assert_eq!(theme::format_shortcut("cmd-shift-left"), "Win+Shift+←");
            assert_eq!(theme::format_shortcut("cmd-shift-right"), "Win+Shift+→");
            assert_eq!(theme::format_shortcut("shift-up"), "Shift+↑");
            assert_eq!(theme::format_shortcut("shift-down"), "Shift+↓");
        } else {
            assert_eq!(theme::format_shortcut("cmd-shift-left"), "Super+Shift+←");
            assert_eq!(theme::format_shortcut("cmd-shift-right"), "Super+Shift+→");
            assert_eq!(theme::format_shortcut("shift-up"), "Shift+↑");
            assert_eq!(theme::format_shortcut("shift-down"), "Shift+↓");
        }
    }

    #[gpui::test]
    fn text_delete_to_line_start_and_end_work_with_default_and_custom_bindings_across_all_text_inputs(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        assert!(config.keyboard.bindings.iter().any(|b| {
            b.keys == "cmd-backspace"
                && b.context == config::KeyContext::TextInput
                && b.action == Command::DeleteToLineStart
        }));
        assert!(
            !config
                .keyboard
                .bindings
                .iter()
                .any(|b| b.action == Command::DeleteToLineEnd)
        );
        config.files.save_session = false;
        config.files.restore_session = false;

        // Add custom TextInput bindings alongside the default `cmd-backspace` for delete_to_line_start.
        config.keyboard.bindings.push(config::Binding {
            keys: "ctrl-u".into(),
            context: config::KeyContext::TextInput,
            action: Command::DeleteToLineStart,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "alt-k".into(),
            context: config::KeyContext::TextInput,
            action: Command::DeleteToLineEnd,
        });
        // Register a Presets-specific override BEFORE a TextInput binding for the same key to
        // ensure context specificity beats JSON order.
        config.keyboard.bindings.push(config::Binding {
            keys: "alt-delete".into(),
            context: config::KeyContext::Presets,
            action: Command::FirstLine,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "alt-delete".into(),
            context: config::KeyContext::TextInput,
            action: Command::DeleteToLineEnd,
        });
        config.validate().unwrap();
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("delete-line.log");
        std::fs::write(&path, "line 0\nline 1\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].selected = 1;
            view.tabs[0].main_selection = LineSelection::single(1);
            cx.notify();
        });
        cx.run_until_parked();

        let caret_range = |input: &Entity<EditableTextState>,
                           window: &mut Window,
                           cx: &mut Context<Workspace>| {
            input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range
        };

        // 1. Search box (`Search` / `TextInput`): default cmd-backspace and custom bindings for
        //    mid-line delete to start/end, boundary no-ops, undo, active selection deletion, and
        //    multibyte Unicode.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("alpha beta gamma");
        cx.simulate_keystrokes("left left left left left left left left left left");
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "beta gamma");
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });
        // At start of line, delete_to_line_start is a safe no-op.
        cx.simulate_keystrokes("cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "beta gamma");
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });

        cx.simulate_keystrokes("right right right right alt-k");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "beta");
            assert_eq!(caret_range(&view.input, window, cx), 4..4);
        });
        // At end of line, delete_to_line_end is a safe no-op.
        cx.simulate_keystrokes("alt-k");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "beta");
            assert_eq!(caret_range(&view.input, window, cx), 4..4);
        });

        // Undo restores the deleted tail cleanly.
        let undo = if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        };
        cx.simulate_keystrokes(undo);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "beta gamma");
        });

        // When a selection exists, delete_to_line_start removes the selection.
        cx.simulate_keystrokes("home right shift-right shift-right shift-right ctrl-u");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "b gamma");
            assert_eq!(caret_range(&view.input, window, cx), 1..1);
        });

        // Multibyte Unicode text.
        view.update_in(cx, |view, _window, cx| {
            view.input.update(cx, |state, cx| {
                state.emplace("שלום мир", cx);
                state.move_to("שלום ".len(), cx);
            });
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-k");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "שלום ");
            let end = "שלום ".encode_utf16().count();
            assert_eq!(caret_range(&view.input, window, cx), end..end);
        });
        cx.simulate_keystrokes("cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "");
            assert_eq!(caret_range(&view.input, window, cx), 0..0);
        });

        // 2. File path bar (`FilePath` / `TextInput`).
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        let initial_path =
            view.read_with(cx, |view, cx| view.path_input.read(cx).as_str().to_owned());
        cx.simulate_keystrokes("home");
        cx.simulate_input("prefix_");
        cx.simulate_keystrokes("cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.path_input.read(cx).as_str(), initial_path);
            assert_eq!(caret_range(&view.path_input, window, cx), 0..0);
        });
        cx.simulate_keystrokes("end");
        cx.simulate_input("_suffix");
        cx.simulate_keystrokes("left left left left left left left alt-k");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.path_input.read(cx).as_str(), initial_path);
            let end = initial_path.encode_utf16().count();
            assert_eq!(caret_range(&view.path_input, window, cx), end..end);
        });

        // 3. Pattern search box (`Presets` / `TextInput`): deletion updates live preset filtering,
        //    and Presets-specific override beats TextInput binding regardless of order.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::TogglePresets, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("err_nomatch");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.filtered_presets.is_empty());
        });
        cx.simulate_keystrokes("left left left left left left left left alt-k");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.presets_search.read(cx).as_str(), "err");
            assert!(!view.filtered_presets.is_empty());
        });
        cx.simulate_keystrokes("left cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.presets_search.read(cx).as_str(), "r");
            assert_eq!(caret_range(&view.presets_search, window, cx), 0..0);
        });
        // Clear filter to restore multiple presets, select index 1, then verify alt-delete runs
        // Presets' FirstLine override instead of TextInput's DeleteToLineEnd.
        cx.simulate_keystrokes("alt-k");
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert_eq!(view.preset_index, 1));
        cx.simulate_input("e");
        cx.simulate_keystrokes("down home alt-delete");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.preset_index, 0);
            assert_eq!(view.presets_search.read(cx).as_str(), "e");
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 4. Go-to-line input (`GoTo` / `TextInput`).
        view.update_in(cx, |view, window, cx| {
            view.command(Command::GoToLine, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("123456");
        cx.simulate_keystrokes("left left left alt-k");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.goto.read(cx).as_str(), "123");
            assert_eq!(caret_range(&view.goto, window, cx), 3..3);
        });
        cx.simulate_keystrokes("left cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.goto.read(cx).as_str(), "3");
            assert_eq!(caret_range(&view.goto, window, cx), 0..0);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 5. Calling delete-to-line commands when no text input is focused is a safe no-op.
        view.update_in(cx, |view, window, cx| {
            assert!(view.focus.is_focused(window));
            view.command(Command::DeleteToLineStart, window, cx);
            view.command(Command::DeleteToLineEnd, window, cx);
            assert_eq!(view.tabs[0].selected, 1);
            assert_eq!(view.goto.read(cx).as_str(), "3");
        });

        // Verify shortcut label formatting for the default delete_to_line_start shortcut.
        if cfg!(target_os = "macos") {
            assert_eq!(theme::format_shortcut("cmd-backspace"), "⌘⌫");
        } else if cfg!(target_os = "windows") {
            assert_eq!(theme::format_shortcut("cmd-backspace"), "Win+Backspace");
        } else {
            assert_eq!(theme::format_shortcut("cmd-backspace"), "Super+Backspace");
        }
    }

    #[gpui::test]
    fn text_delete_to_line_shortcuts_come_only_from_keyboard_config(cx: &mut TestAppContext) {
        let filtered_defaults = text_input_default_bindings();
        assert!(!filtered_defaults.iter().any(|binding| {
            binding
                .action()
                .partial_eq(&input_actions::DeleteToLineStart)
                || binding.action().partial_eq(&input_actions::DeleteToLineEnd)
        }));
        // Other standard editable-text bindings remain intact.
        assert!(
            filtered_defaults
                .iter()
                .any(|binding| { binding.action().partial_eq(&input_actions::DeleteLeft) })
        );
        assert!(
            filtered_defaults
                .iter()
                .any(|binding| { binding.action().partial_eq(&input_actions::Copy) })
        );

        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(filtered_defaults.clone());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("default-delete.log");
        std::fs::write(&path, "line 0\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());

        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(config.clone(), vec![path.clone()], window, cx)
        });
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log.clone());
            cx.notify();
        });
        cx.run_until_parked();

        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("alpha beta");
        cx.simulate_keystrokes("left left left left left");
        cx.run_until_parked();

        // Unconfigured GPUI built-in line-delete shortcuts and ctrl-u should not mutate text.
        for stroke in [
            "ctrl-k",
            "ctrl-shift-backspace",
            "ctrl-shift-delete",
            "ctrl-u",
        ] {
            cx.simulate_keystrokes(stroke);
            cx.run_until_parked();
            view.update_in(cx, |view, window, cx| {
                assert_eq!(
                    view.input.read(cx).as_str(),
                    "alpha beta",
                    "default binding {stroke} unexpectedly deleted text"
                );
                let range = view
                    .input
                    .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                    .expect("selection range")
                    .range;
                assert_eq!(
                    range,
                    5..5,
                    "default binding {stroke} unexpectedly moved caret"
                );
            });
        }

        // The default `cmd-backspace` binding in keyboard.json deletes to line start.
        cx.simulate_keystrokes("cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), " beta");
            let range = view
                .input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range;
            assert_eq!(range, 0..0);
        });

        // Removing `delete_to_line_start` from keyboard.json disables `cmd-backspace` too.
        let mut unbound_config = config;
        unbound_config
            .keyboard
            .bindings
            .retain(|b| b.action != Command::DeleteToLineStart);
        cx.update(|_window, cx| {
            cx.clear_key_bindings();
            cx.bind_keys(filtered_defaults);
            cx.bind_keys(keybindings(&unbound_config).unwrap());
        });
        view.update_in(cx, |view, _window, cx| {
            view.input.update(cx, |state, cx| {
                state.emplace("alpha beta", cx);
                state.move_to(5, cx);
            });
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-backspace");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.input.read(cx).as_str(), "alpha beta");
            let range = view
                .input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range;
            assert_eq!(range, 5..5);
        });
    }

    #[gpui::test]
    fn first_and_last_line_shortcuts_work_in_log_filter_presets_and_file_path_menus(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.search.presets.push(config::Preset {
            name: "HTTP 5xx Errors".into(),
            pattern: "HTTP 5\\d\\d".into(),
            mode: SearchMode::Regex,
            highlight_match: true,
            select_text: None,
        });
        config.search.presets.push(config::Preset {
            name: "Auth Errors".into(),
            pattern: "401|403".into(),
            mode: SearchMode::Regex,
            highlight_match: true,
            select_text: None,
        });
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let alpha = root.join("alpha.log");
        let beta = root.join("beta.log");
        let gamma = root.join("gamma.log");
        let zeta = root.join("zeta.log");
        std::fs::write(
            &alpha,
            "match line 0\nplain line 1\nmatch line 2\nplain line 3\nmatch line 4\nmatch line 5\n",
        )
        .unwrap();
        for file in [&beta, &gamma, &zeta] {
            std::fs::write(file, "entry\n").unwrap();
        }
        let dir_prefix = ensure_trailing_separator(&root.display().to_string());

        let (first_key, last_key, select_first_key, select_last_key, presets_key) =
            if cfg!(target_os = "macos") {
                (
                    "cmd-up",
                    "cmd-down",
                    "cmd-shift-up",
                    "cmd-shift-down",
                    "cmd-p",
                )
            } else {
                (
                    "ctrl-up",
                    "ctrl-down",
                    "ctrl-shift-up",
                    "ctrl-shift-down",
                    "ctrl-p",
                )
            };

        let (view, cx) = cx
            .add_window_view(|window, cx| Workspace::new(config, vec![alpha.clone()], window, cx));
        wait_for_tab_loaded(&view, 0, cx);

        // 1. Main log pane (`Log`): secondary-up/down jump to first/last line,
        //    secondary-shift-up/down extend selection to first/last line (including reversal).
        view.update(cx, |view, cx| {
            view.tabs[0].selected = 2;
            view.tabs[0].main_selection = LineSelection::single(2);
            view.tabs[0].follow = true;
            cx.notify();
        });
        cx.run_until_parked();

        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 5);
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(5, 5)]);
            assert!(!view.tabs[0].follow);
        });

        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 0);
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(0, 0)]);
        });

        cx.simulate_keystrokes("down down");
        cx.run_until_parked();
        cx.simulate_keystrokes(select_last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 5);
            assert_eq!(view.tabs[0].main_selection.anchor, 2);
            assert_eq!(view.tabs[0].main_selection.cursor, 5);
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(2, 5)]);
        });

        cx.simulate_keystrokes(select_first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 0);
            assert_eq!(view.tabs[0].main_selection.anchor, 2);
            assert_eq!(view.tabs[0].main_selection.cursor, 0);
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(0, 2)]);
        });

        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].selected, 5);
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(5, 5)]);
        });

        // Existing home/end and shift-home/shift-end remain intact in Log.
        cx.simulate_keystrokes("home down shift-end");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(1, 5)]);
        });
        cx.simulate_keystrokes("shift-home");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(0, 1)]);
        });

        // 2. Filter results pane (`Log` / `FilterLog`): secondary-up/down and
        //    secondary-shift-up/down navigate and extend selection within visible results.
        view.update_in(cx, |view, window, cx| {
            view.tabs[0].results = SearchResults {
                matches: vec![0, 2, 4, 5],
                visible: vec![0, 2, 4, 5],
                ..SearchResults::default()
            };
            view.tabs[0].result_selection = LineSelection::single(1);
            view.focus_pane(Pane::Results, window, cx);
            cx.notify();
        });
        cx.run_until_parked();

        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(3, 3)]);
            assert_eq!(view.tabs[0].result_selection.cursor, 3);
            assert_eq!(view.tabs[0].selected, 5);
        });

        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(0, 0)]);
            assert_eq!(view.tabs[0].result_selection.cursor, 0);
            assert_eq!(view.tabs[0].selected, 0);
        });

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        cx.simulate_keystrokes(select_last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.anchor, 1);
            assert_eq!(view.tabs[0].result_selection.cursor, 3);
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(1, 3)]);
            assert_eq!(view.tabs[0].selected, 5);
        });

        cx.simulate_keystrokes(select_first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.anchor, 1);
            assert_eq!(view.tabs[0].result_selection.cursor, 0);
            assert_eq!(view.tabs[0].result_selection.ranges(), &[(0, 1)]);
            assert_eq!(view.tabs[0].selected, 0);
        });

        // Return focus to main pane at line 2 before testing menus.
        view.update_in(cx, |view, window, cx| {
            view.tabs[0].selected = 2;
            view.tabs[0].main_selection = LineSelection::single(2);
            view.focus_pane(Pane::Main, window, cx);
            cx.notify();
        });
        cx.run_until_parked();

        // 3. Saved patterns menu (`Presets`): secondary-up/down jump to first/last preset
        //    (both unfiltered and filtered) without moving log selection or mutating filter text.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_presets);
            assert_eq!(view.filtered_presets.len(), 5);
            assert_eq!(view.preset_index, 0);
        });

        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.preset_index, 4);
            assert_eq!(view.tabs[0].selected, 2);
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(2, 2)]);
        });
        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.preset_index, 0);
            assert_eq!(view.tabs[0].selected, 2);
        });

        cx.simulate_input("err");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.filtered_presets.len(), 3);
            assert_eq!(view.preset_index, 0);
        });
        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.preset_index, 2);
            assert_eq!(view.presets_search.read(cx).as_str(), "err");
            assert_eq!(view.tabs[0].selected, 2);
        });
        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.preset_index, 0);
            assert_eq!(view.presets_search.read(cx).as_str(), "err");
        });
        // Log-only select-to-first/last shortcuts do not leak into the Log pane while in Presets.
        cx.simulate_keystrokes(select_last_key);
        cx.simulate_keystrokes(select_first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(2, 2)]);
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 4. File path input & completions menu (`FilePath`): secondary-up/down jump to
        //    first/last completion (opening the menu if closed) without touching log selection.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&dir_prefix);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_path_completions);
            assert_eq!(view.path_completions.items.len(), 4);
            assert_eq!(view.path_completion_index, None);
        });

        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.path_completion_index, Some(3));
            assert_eq!(view.path_completions.items[3].label, "zeta.log");
            assert_eq!(view.tabs[0].selected, 2);
        });
        // Repeating last_key stays clamped at the last item (does not wrap).
        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.path_completion_index, Some(3));
        });

        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.path_completion_index, Some(0));
            assert_eq!(view.path_completions.items[0].label, "alpha.log");
        });
        // Repeating first_key stays clamped at the first item (does not wrap).
        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.path_completion_index, Some(0));
        });

        // With completions dismissed (first Escape), last_key/first_key re-open and jump directly.
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.path_input_focused(window, cx));
            assert!(!view.show_path_completions);
        });
        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_path_completions);
            assert_eq!(view.path_completion_index, Some(3));
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.show_path_completions);
            assert_eq!(view.path_completion_index, Some(0));
        });

        // Log-only select-to-first/last shortcuts do not leak into the Log pane while in FilePath.
        cx.simulate_keystrokes(select_last_key);
        cx.simulate_keystrokes(select_first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].main_selection.ranges(), &[(2, 2)]);
        });

        // Jumping to last completion and pressing Enter opens zeta.log.
        cx.simulate_keystrokes(last_key);
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 0, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].path, zeta);
            assert!(!view.show_path_completions);
        });

        // With no matching completions, first/last shortcuts are safe no-ops.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusFilePath, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input(&format!("{dir_prefix}does_not_exist_"));
        cx.run_until_parked();
        cx.simulate_keystrokes(last_key);
        cx.simulate_keystrokes(first_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.show_path_completions);
            assert_eq!(view.path_completion_index, None);
        });

        // 5. Shortcut label formatting matches platform conventions.
        if cfg!(target_os = "macos") {
            assert_eq!(theme::format_shortcut("secondary-up"), "⌘↑");
            assert_eq!(theme::format_shortcut("secondary-down"), "⌘↓");
            assert_eq!(theme::format_shortcut("secondary-shift-up"), "⌘⇧↑");
            assert_eq!(theme::format_shortcut("secondary-shift-down"), "⌘⇧↓");
        } else {
            assert_eq!(theme::format_shortcut("secondary-up"), "Ctrl+↑");
            assert_eq!(theme::format_shortcut("secondary-down"), "Ctrl+↓");
            assert_eq!(theme::format_shortcut("secondary-shift-up"), "Ctrl+Shift+↑");
            assert_eq!(
                theme::format_shortcut("secondary-shift-down"),
                "Ctrl+Shift+↓"
            );
        }
    }

    #[gpui::test]
    fn sticky_gutter_stays_pinned_and_interactive_across_horizontal_scroll_in_both_panes(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.line_numbers = true;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sticky-gutter.log");
        let wide_body = (0..40)
            .map(|chunk| format!("TOKEN_{chunk:02}_PAYLOAD "))
            .collect::<String>();
        let mut content = String::new();
        for i in 0..80 {
            if i == 1 {
                content.push_str("short line\n");
            } else {
                content.push_str(&format!("MATCH line_{i:02} START_PREFIX {wide_body}END\n"));
            }
        }
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let visible: Vec<usize> = (0..80).filter(|&i| i != 1).collect();
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
            view.tabs[0].sync_highlight_regex();
            view.tabs[0].results = results;
            view.tabs[0].selected = 0;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.tabs[0].result_selection = LineSelection::single(0);
            cx.notify();
        });
        cx.run_until_parked();

        let expected_gutter_w =
            view.read_with(cx, |view, _| view.zoom_metrics().sticky_gutter_width(true));
        let main_gutter0_before = cx
            .debug_bounds("main-gutter-0")
            .expect("main gutter 0 bounds");
        let main_mark0_before = cx.debug_bounds("main-mark-0").expect("main mark 0 bounds");
        let main_num0_before = cx
            .debug_bounds("main-linenum-0")
            .expect("main line number 0 bounds");
        let main_text0_before = cx.debug_bounds("main-text-0").expect("main text 0 bounds");
        let main_hscroll_before = cx
            .debug_bounds("main-hscroll")
            .expect("main horizontal scrollbar bounds");
        let res_gutter0_before = cx
            .debug_bounds("result-gutter-0")
            .expect("result gutter 0 bounds");
        let res_mark0_before = cx
            .debug_bounds("result-mark-0")
            .expect("result mark 0 bounds");
        let res_num0_before = cx
            .debug_bounds("result-linenum-0")
            .expect("result line number 0 bounds");
        let res_text0_before = cx
            .debug_bounds("result-text-0")
            .expect("result text 0 bounds");
        let res_hscroll_before = cx
            .debug_bounds("results-hscroll")
            .expect("results horizontal scrollbar bounds");

        assert!(
            (f32::from(main_gutter0_before.size.width) - expected_gutter_w).abs() < 0.5,
            "expected gutter width {expected_gutter_w}, got {:?}",
            main_gutter0_before.size.width
        );
        assert!(
            (f32::from(main_hscroll_before.origin.x) - f32::from(main_gutter0_before.right()))
                .abs()
                < 0.5,
            "main horizontal scrollbar should begin flush with sticky gutter right edge"
        );
        assert!(
            (f32::from(res_hscroll_before.origin.x) - f32::from(res_gutter0_before.right())).abs()
                < 0.5,
            "results horizontal scrollbar should begin flush with sticky gutter right edge"
        );

        // 1. Scroll horizontally via Right arrow keys in Main pane: text moves left, gutter stays pinned.
        for _ in 0..5 {
            cx.simulate_keystrokes("right");
        }
        cx.run_until_parked();

        let main_offset_after_keys = view.read_with(cx, |view, _| {
            f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x)
        });
        assert!(
            main_offset_after_keys < -150.0,
            "expected horizontal scroll via right arrow, got {main_offset_after_keys}"
        );
        let main_gutter0_scrolled = cx.debug_bounds("main-gutter-0").unwrap();
        let main_gutter1_scrolled = cx.debug_bounds("main-gutter-1").unwrap();
        let main_mark0_scrolled = cx.debug_bounds("main-mark-0").unwrap();
        let main_num0_scrolled = cx.debug_bounds("main-linenum-0").unwrap();
        let main_text0_scrolled = cx.debug_bounds("main-text-0").unwrap();

        assert!(
            (f32::from(main_gutter0_scrolled.origin.x) - f32::from(main_gutter0_before.origin.x))
                .abs()
                < 0.5,
            "main gutter 0 must remain pinned during horizontal scroll"
        );
        assert!(
            (f32::from(main_gutter1_scrolled.origin.x) - f32::from(main_gutter0_before.origin.x))
                .abs()
                < 0.5,
            "short row gutter must remain pinned during horizontal scroll"
        );
        assert!(
            (f32::from(main_mark0_scrolled.origin.x) - f32::from(main_mark0_before.origin.x)).abs()
                < 0.5,
            "main marker/match column must remain pinned during horizontal scroll"
        );
        assert!(
            (f32::from(main_num0_scrolled.origin.x) - f32::from(main_num0_before.origin.x)).abs()
                < 0.5,
            "main line number column must remain pinned during horizontal scroll"
        );
        assert!(
            f32::from(main_text0_scrolled.origin.x) < f32::from(main_text0_before.origin.x) - 100.0,
            "log text should scroll left behind the sticky gutter"
        );

        // 2. Scroll wheel horizontal & vertical input directly over the sticky gutter works and keeps gutter pinned.
        cx.simulate_event(ScrollWheelEvent {
            position: main_num0_scrolled.center(),
            delta: ScrollDelta::Pixels(point(px(-96.0), px(0.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.run_until_parked();
        let main_offset_after_wheel_x = view.read_with(cx, |view, _| {
            f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x)
        });
        assert!(
            main_offset_after_wheel_x < main_offset_after_keys - 50.0,
            "horizontal wheel scrolling over sticky gutter should scroll the pane"
        );
        assert!(
            (f32::from(cx.debug_bounds("main-gutter-0").unwrap().origin.x)
                - f32::from(main_gutter0_before.origin.x))
            .abs()
                < 0.5
        );

        // 3. Clicking the sticky mark gutter while text is scrolled behind it toggles bookmark without selecting text.
        let mark2_pos = cx.debug_bounds("main-mark-2").unwrap().center();
        cx.simulate_click(mark2_pos, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(
                view.tabs[0].bookmarks.contains(&2),
                "clicking pinned mark gutter should bookmark line 2"
            );
            assert!(
                !view.tabs[0].has_text_selection(Pane::Main),
                "clicking pinned mark gutter must not select scrolled-behind text"
            );
        });
        cx.simulate_click(mark2_pos, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].bookmarks.contains(&2));
        });

        // 4. Clicking and Shift+clicking sticky line numbers selects rows without triggering text selection.
        let num3_pos = cx.debug_bounds("main-linenum-3").unwrap().center();
        let num5_pos = cx.debug_bounds("main-linenum-5").unwrap().center();
        cx.simulate_click(num3_pos, Modifiers::none());
        cx.simulate_click(num5_pos, Modifiers::shift());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.tabs[0].main_selection.iter().collect::<Vec<_>>(),
                vec![3, 4, 5],
                "clicking sticky line numbers must support single and Shift+click row selection"
            );
            assert!(
                !view.tabs[0].has_text_selection(Pane::Main),
                "clicking sticky line numbers must not trigger hidden text selection"
            );
        });

        // 5. Dragging text selection from visible text leftward into the sticky gutter clamps to the visible text edge.
        let gutter0_right = cx.debug_bounds("main-gutter-0").unwrap().right();
        let row0_y = cx.debug_bounds("main-gutter-0").unwrap().center().y;
        let drag_start = point(gutter0_right + px(90.0), row0_y);
        let drag_into_gutter = point(main_num0_scrolled.center().x, row0_y);
        cx.simulate_mouse_down(drag_start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(drag_into_gutter, Some(MouseButton::Left), Modifiers::none());
        cx.simulate_mouse_up(drag_into_gutter, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        let copy_key = if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-c"
        };
        cx.simulate_keystrokes(copy_key);
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("clipboard should contain visible selected substring");
        assert!(
            !copied.is_empty() && !copied.contains("START_PREFIX"),
            "dragging into sticky gutter must clamp to visible text and not copy scrolled-off prefix, got {copied:?}"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // 6. Vertical scroll while horizontally scrolled keeps newly revealed rows' gutters pinned.
        let vertical_wheel_pos = cx.debug_bounds("main-linenum-0").unwrap().center();
        cx.simulate_event(ScrollWheelEvent {
            position: vertical_wheel_pos,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-240.0))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        cx.run_until_parked();
        let scrolled_in_gutter = cx
            .debug_bounds("main-gutter-12")
            .expect("row 12 should be visible after vertical wheel scroll over sticky gutter");
        assert!(
            (f32::from(scrolled_in_gutter.origin.x) - f32::from(main_gutter0_before.origin.x))
                .abs()
                < 0.5,
            "newly scrolled-in row gutter must also remain pinned horizontally"
        );

        // 7. Clicking horizontal scrollbar moves further right while keeping gutter pinned.
        let hbounds = cx.debug_bounds("main-hscroll").unwrap();
        let hclick = point(
            hbounds.origin.x + hbounds.size.width * 0.85,
            hbounds.center().y,
        );
        cx.simulate_click(hclick, Modifiers::none());
        cx.run_until_parked();
        assert!(
            (f32::from(cx.debug_bounds("main-gutter-12").unwrap().origin.x)
                - f32::from(main_gutter0_before.origin.x))
            .abs()
                < 0.5,
            "gutter must remain pinned after horizontal scrollbar click"
        );

        // 8. Results (filter) pane: scrolling horizontally keeps its marker + line-number gutter pinned & clickable.
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        for _ in 0..5 {
            cx.simulate_keystrokes("right");
        }
        cx.run_until_parked();

        let res_gutter0_scrolled = cx.debug_bounds("result-gutter-0").unwrap();
        let res_mark0_scrolled = cx.debug_bounds("result-mark-0").unwrap();
        let res_num0_scrolled = cx.debug_bounds("result-linenum-0").unwrap();
        let res_text0_scrolled = cx.debug_bounds("result-text-0").unwrap();
        assert!(
            (f32::from(res_gutter0_scrolled.origin.x) - f32::from(res_gutter0_before.origin.x))
                .abs()
                < 0.5,
            "results gutter must remain pinned during horizontal scroll"
        );
        assert!(
            (f32::from(res_mark0_scrolled.origin.x) - f32::from(res_mark0_before.origin.x)).abs()
                < 0.5
        );
        assert!(
            (f32::from(res_num0_scrolled.origin.x) - f32::from(res_num0_before.origin.x)).abs()
                < 0.5
        );
        assert!(
            f32::from(res_text0_scrolled.origin.x) < f32::from(res_text0_before.origin.x) - 100.0
        );

        cx.simulate_click(res_mark0_scrolled.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].bookmarks.contains(&0));
            assert!(!view.tabs[0].has_text_selection(Pane::Results));
        });

        let res_num1 = cx.debug_bounds("result-linenum-1").unwrap().center();
        cx.simulate_click(res_num1, Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs[0].result_selection.cursor, 1);
            assert!(!view.tabs[0].has_text_selection(Pane::Results));
        });

        // 9. Zooming while horizontally scrolled scales gutter width and keeps gutter + scrollbar aligned.
        let zoom_in_key = if cfg!(target_os = "macos") {
            "cmd-="
        } else {
            "ctrl-="
        };
        cx.simulate_keystrokes(zoom_in_key);
        cx.simulate_keystrokes(zoom_in_key);
        cx.run_until_parked();
        let zoomed_expected_w =
            view.read_with(cx, |view, _| view.zoom_metrics().sticky_gutter_width(true));
        let zoomed_res_gutter = cx.debug_bounds("result-gutter-0").unwrap();
        let zoomed_res_hscroll = cx.debug_bounds("results-hscroll").unwrap();
        assert!(
            (f32::from(zoomed_res_gutter.origin.x) - f32::from(res_gutter0_before.origin.x)).abs()
                < 0.5
        );
        assert!(
            (f32::from(zoomed_res_gutter.size.width) - zoomed_expected_w).abs() < 0.5,
            "zoomed gutter width should match sticky_gutter_width"
        );
        assert!(
            (f32::from(zoomed_res_hscroll.origin.x) - f32::from(zoomed_res_gutter.right())).abs()
                < 0.5,
            "zoomed horizontal scrollbar should remain flush with zoomed sticky gutter"
        );
    }

    #[gpui::test]
    fn sticky_mark_gutter_stays_pinned_when_line_numbers_hidden(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.line_numbers = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("no-linenum.log");
        let wide_payload = "x".repeat(320);
        std::fs::write(
            &path,
            format!("MATCH row0 {wide_payload}\nMATCH row1 {wide_payload}\n"),
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

        assert!(cx.debug_bounds("main-linenum-0").is_none());
        let gutter_before = cx.debug_bounds("main-gutter-0").unwrap();
        let mark_before = cx.debug_bounds("main-mark-0").unwrap();
        let text_before = cx.debug_bounds("main-text-0").unwrap();
        let hscroll = cx.debug_bounds("main-hscroll").unwrap();
        let expected_w =
            view.read_with(cx, |view, _| view.zoom_metrics().sticky_gutter_width(false));
        assert!((f32::from(gutter_before.size.width) - expected_w).abs() < 0.5);
        assert!((f32::from(hscroll.origin.x) - f32::from(gutter_before.right())).abs() < 0.5);

        for _ in 0..4 {
            cx.simulate_keystrokes("right");
        }
        cx.run_until_parked();

        let gutter_after = cx.debug_bounds("main-gutter-0").unwrap();
        let mark_after = cx.debug_bounds("main-mark-0").unwrap();
        let text_after = cx.debug_bounds("main-text-0").unwrap();
        assert!((f32::from(gutter_after.origin.x) - f32::from(gutter_before.origin.x)).abs() < 0.5);
        assert!((f32::from(mark_after.origin.x) - f32::from(mark_before.origin.x)).abs() < 0.5);
        assert!(f32::from(text_after.origin.x) < f32::from(text_before.origin.x) - 100.0);

        // Clicking the pinned bookmark column while text is scrolled underneath toggles bookmark cleanly.
        cx.simulate_click(mark_after.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(view.tabs[0].bookmarks.contains(&0));
            assert!(!view.tabs[0].has_text_selection(Pane::Main));
        });
    }

    #[gpui::test]
    fn secondary_left_resets_horizontal_scroll_in_active_log_pane_only(cx: &mut TestAppContext) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reset-hscroll.log");
        let long_payload = "x".repeat(320);
        let content = (0..80)
            .map(|i| format!("MATCH line {i} {long_payload}\n"))
            .collect::<String>();
        std::fs::write(&path, content).unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let visible: Vec<usize> = (0..80).collect();
        let results = SearchResults {
            matches: visible.clone(),
            visible,
            limited: false,
            widest_row: 0,
        };

        let reset_key = if cfg!(target_os = "macos") {
            "cmd-left"
        } else {
            "ctrl-left"
        };

        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].query = "MATCH".into();
            view.tabs[0].results = results;
            view.tabs[0].selected = 2;
            view.tabs[0].follow = true;
            view.tabs[0].main_selection = LineSelection::single(2);
            view.tabs[0].result_selection = LineSelection::single(1);
            cx.notify();
        });
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            let expected_label = if cfg!(target_os = "macos") {
                "⌘←"
            } else {
                "Ctrl+←"
            };
            assert_eq!(
                view.theme
                    .shortcut(Command::ResetHorizontalScroll)
                    .as_deref(),
                Some(expected_label)
            );
        });

        // No-op when already at horizontal offset 0.
        cx.simulate_keystrokes(reset_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x),
                0.0
            );
        });

        // Scroll both panes horizontally and vertically so we can verify active-pane isolation
        // and vertical scroll / selection / follow preservation.
        view.update(cx, |view, cx| {
            set_uniform_list_axis_offset(&view.tabs[0].scroll, ScrollAxis::Vertical, 120.0);
            set_uniform_list_axis_offset(&view.tabs[0].scroll, ScrollAxis::Horizontal, 300.0);
            set_uniform_list_axis_offset(&view.tabs[0].result_scroll, ScrollAxis::Vertical, 72.0);
            set_uniform_list_axis_offset(
                &view.tabs[0].result_scroll,
                ScrollAxis::Horizontal,
                220.0,
            );
            cx.notify();
        });
        cx.run_until_parked();

        // In Main pane: secondary-left resets only main horizontal scroll back to 0.
        cx.simulate_keystrokes(reset_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let main_offset = view.tabs[0].scroll.0.borrow().base_handle.offset();
            let res_offset = view.tabs[0].result_scroll.0.borrow().base_handle.offset();
            assert_eq!(f32::from(main_offset.x), 0.0);
            assert!((f32::from(main_offset.y) + 120.0).abs() < 0.5);
            assert_eq!(view.tabs[0].selected, 2);
            assert!(view.tabs[0].follow);
            assert!(
                (f32::from(res_offset.x) + 220.0).abs() < 0.5,
                "resetting main pane must not move results pane"
            );
            let main_hlayout = view
                .main_hscroll_state
                .layout()
                .expect("main horizontal scrollbar layout");
            assert_eq!(main_hlayout.thumb.start, 0.0);
        });

        // Re-scroll main pane, switch to Results pane, and verify secondary-left resets only results.
        view.update(cx, |view, cx| {
            set_uniform_list_axis_offset(&view.tabs[0].scroll, ScrollAxis::Horizontal, 180.0);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        cx.simulate_keystrokes(reset_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let main_offset = view.tabs[0].scroll.0.borrow().base_handle.offset();
            let res_offset = view.tabs[0].result_scroll.0.borrow().base_handle.offset();
            assert_eq!(f32::from(res_offset.x), 0.0);
            assert!((f32::from(res_offset.y) + 72.0).abs() < 0.5);
            assert!(
                (f32::from(main_offset.x) + 180.0).abs() < 0.5,
                "resetting results pane must not move main pane"
            );
            let res_hlayout = view
                .results_hscroll_state
                .layout()
                .expect("results horizontal scrollbar layout");
            assert_eq!(res_hlayout.thumb.start, 0.0);
        });

        // In Search input, cmd-left moves the text caret to line start and never resets log scroll.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("abc");
        cx.simulate_keystrokes("cmd-left");
        cx.simulate_input("Z");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "Zabc");
            assert!(
                (f32::from(view.tabs[0].scroll.0.borrow().base_handle.offset().x) + 180.0).abs()
                    < 0.5,
                "cmd-left in Search input must not reset log horizontal scroll"
            );
        });
    }

    #[gpui::test]
    fn restore_closed_tab_reopens_closed_tabs_in_lifo_order_with_state_and_position(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let restore_key = if cfg!(target_os = "macos") {
            "cmd-shift-t"
        } else {
            "ctrl-shift-t"
        };
        let close_key = if cfg!(target_os = "macos") {
            "cmd-w"
        } else {
            "ctrl-w"
        };

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        let third = directory.path().join("third.log");
        std::fs::write(&first, "alpha\nERROR one\nomega\n").unwrap();
        std::fs::write(&second, "beta\nWARN two\nWARN three\n").unwrap();
        std::fs::write(&third, "gamma\nINFO four\n").unwrap();
        let first_canon = first.canonicalize().unwrap();
        let second_canon = second.canonicalize().unwrap();
        let third_canon = third.canonicalize().unwrap();

        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new(
                config,
                vec![first.clone(), second.clone(), third.clone()],
                window,
                cx,
            )
        });
        for index in 0..3 {
            wait_for_tab_loaded(&view, index, cx);
        }

        // Default shortcut label is populated for RestoreClosedTab.
        view.read_with(cx, |view, _| {
            let expected = if cfg!(target_os = "macos") {
                "⌘⇧T"
            } else {
                "Ctrl+Shift+T"
            };
            assert_eq!(
                view.theme.shortcut(Command::RestoreClosedTab).as_deref(),
                Some(expected)
            );
        });

        // No-op when no tab has been closed yet.
        cx.simulate_keystrokes(restore_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 3);
            assert_eq!(view.active, 2);
        });

        // Customize state on all three tabs and make tab 1 (second.log) active.
        view.update(cx, |view, cx| {
            view.tabs[0].query = "ERROR".into();
            view.tabs[0].options.mode = SearchMode::Literal;
            view.tabs[0].options.case_sensitive = true;
            view.tabs[0].bookmarks = BTreeSet::from([0, 2]);
            view.tabs[0].selected = 1;
            view.tabs[0].follow = false;

            view.tabs[1].query = "WARN".into();
            view.tabs[1].options.mode = SearchMode::Regex;
            view.tabs[1].options.whole_word = true;
            view.tabs[1].options.invert = false;
            view.tabs[1].options.highlight_matches = false;
            view.tabs[1].bookmarks = BTreeSet::from([2]);
            view.tabs[1].selected = 2;
            view.tabs[1].follow = true;

            view.tabs[2].query = "INFO".into();
            view.tabs[2].selected = 1;

            view.switch(1, cx);
            // Edit the active tab's search input without committing a search yet to ensure
            // closing captures the live input value.
            view.input.update(cx, |input, cx| input.emplace("WARN", cx));
        });
        cx.run_until_parked();

        // Close middle tab (index 1, second.log) with secondary-w.
        cx.simulate_keystrokes(close_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.tabs[0].path, first_canon);
            assert_eq!(view.tabs[1].path, third_canon);
        });

        // Switch to first.log (index 0), type an updated query in search input, and close it.
        view.update(cx, |view, cx| {
            view.switch(0, cx);
            view.input
                .update(cx, |input, cx| input.emplace("ERROR", cx));
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(close_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 1);
            assert_eq!(view.tabs[0].path, third_canon);
        });

        // First secondary-shift-t restores first.log (LIFO) at its original index 0 with state.
        cx.simulate_keystrokes(restore_key);
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 0, cx);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(view.active, 0);
            assert_eq!(view.tabs[0].path, first_canon);
            assert_eq!(view.tabs[1].path, third_canon);
            assert_eq!(view.tabs[0].query, "ERROR");
            assert_eq!(view.input.read(cx).as_str(), "ERROR");
            assert_eq!(
                view.path_input.read(cx).as_str(),
                first_canon.display().to_string()
            );
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
            assert!(view.tabs[0].options.case_sensitive);
            assert_eq!(view.tabs[0].bookmarks, BTreeSet::from([0, 2]));
            assert_eq!(view.tabs[0].selected, 1);
            assert!(!view.tabs[0].follow);
            assert_eq!(view.tabs[0].results.matches, vec![1]);
        });

        // Works while search input is focused (Workspace context): second secondary-shift-t
        // restores second.log at its original index 1 between first.log and third.log.
        view.update_in(cx, |view, window, cx| {
            view.command(Command::FocusSearch, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(restore_key);
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 1, cx);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs.len(), 3);
            assert_eq!(view.active, 1);
            assert_eq!(view.tabs[0].path, first_canon);
            assert_eq!(view.tabs[1].path, second_canon);
            assert_eq!(view.tabs[2].path, third_canon);
            assert_eq!(view.tabs[1].query, "WARN");
            assert_eq!(view.input.read(cx).as_str(), "WARN");
            assert_eq!(
                view.path_input.read(cx).as_str(),
                second_canon.display().to_string()
            );
            assert_eq!(view.tabs[1].options.mode, SearchMode::Regex);
            assert!(view.tabs[1].options.whole_word);
            assert!(!view.tabs[1].options.highlight_matches);
            assert_eq!(view.tabs[1].bookmarks, BTreeSet::from([2]));
            assert_eq!(view.tabs[1].selected, 2);
            assert!(view.tabs[1].follow);
            assert_eq!(view.tabs[1].results.matches, vec![1, 2]);
            // Previously active tab's query (first.log) was preserved when switching.
            assert_eq!(view.tabs[0].query, "ERROR");
        });

        // Close all remaining tabs down to the empty workspace, then restore from empty state.
        view.update_in(cx, |view, window, cx| {
            view.focus_pane(Pane::Main, window, cx);
        });
        cx.run_until_parked();
        for _ in 0..3 {
            cx.simulate_keystrokes(close_key);
            cx.run_until_parked();
        }
        view.read_with(cx, |view, cx| {
            assert!(view.tabs.is_empty());
            assert_eq!(view.input.read(cx).as_str(), "");
        });

        cx.simulate_keystrokes(restore_key);
        cx.run_until_parked();
        wait_for_tab_loaded(&view, 0, cx);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.tabs.len(), 1);
            assert_eq!(view.active, 0);
            assert_eq!(view.tabs[0].path, first_canon);
            assert_eq!(view.tabs[0].query, "ERROR");
            assert_eq!(view.input.read(cx).as_str(), "ERROR");
            assert_eq!(view.tabs[0].results.matches, vec![1]);
        });

        // If a closed tab was manually reopened in the meantime, RestoreClosedTab skips it and
        // restores the next still-closed tab without duplicating the already-open file.
        // Remaining closed stack has [second.log, third.log] (third.log on top).
        view.update(cx, |view, cx| {
            view.open(third.clone(), cx);
        });
        wait_for_tab_loaded(&view, 1, cx);
        cx.simulate_keystrokes(restore_key);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 3);
            let paths: Vec<_> = view.tabs.iter().map(|tab| tab.path.clone()).collect();
            assert!(paths.contains(&first_canon));
            assert!(paths.contains(&second_canon));
            assert!(paths.contains(&third_canon));
            assert_eq!(view.tabs[view.active].path, second_canon);
        });
    }

    #[gpui::test]
    fn stdin_tab_streams_live_input_preserves_bookmarks_and_cleans_up_on_replace(
        cx: &mut TestAppContext,
    ) {
        use std::io::Write as _;

        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.viewer.poll_interval_ms = 10;
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        let (reader, mut writer) = std::io::pipe().unwrap();
        let spool = StdinSpool::spawn_reader(reader).unwrap();
        let spool_path = spool.path().to_path_buf();

        let (view, vcx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_inputs(config.clone(), vec![InputSource::Stdin(spool)], window, cx)
        });
        vcx.run_until_parked();

        // Wait for initial load of the empty stream to complete.
        let deadline = Instant::now() + Duration::from_secs(2);
        while view.read_with(vcx, |view, _| view.tabs[0].log.is_none()) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for initial stdin load"
            );
            std::thread::sleep(Duration::from_millis(5));
            view.update(vcx, |view, cx| view.tick(cx));
            vcx.run_until_parked();
        }

        view.read_with(vcx, |view, cx| {
            assert_eq!(view.tabs.len(), 1);
            let tab = &view.tabs[0];
            assert!(tab.is_stdin());
            assert!(tab.is_streaming_stdin());
            assert_eq!(tab.label(), "stdin");
            assert_eq!(tab.path_bar_placeholder(), "Standard input (streaming…)");
            assert_eq!(view.path_input.read(cx).as_str(), "");
            assert!(view.recent.is_empty());
            assert_eq!(tab.log.as_ref().unwrap().len(), 0);
        });

        // Write first chunk while keeping the pipe open.
        writer.write_all(b"INFO boot\nERROR timeout\n").unwrap();
        writer.flush().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while view.read_with(vcx, |view, _| {
            view.tabs[0].log.as_ref().map_or(0, |log| log.len()) < 2
        }) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for first streamed lines"
            );
            std::thread::sleep(Duration::from_millis(5));
            view.update(vcx, |view, cx| view.tick(cx));
            vcx.run_until_parked();
        }

        // Bookmark line 0 and search for ERROR.
        view.update(vcx, |view, cx| {
            view.tabs[0].bookmarks.insert(0);
            view.input
                .update(cx, |input, cx| input.emplace("ERROR", cx));
            view.start_search(0, cx);
        });
        vcx.run_until_parked();
        let deadline = Instant::now() + Duration::from_secs(2);
        while view.read_with(vcx, |view, _| view.tabs[0].searching) {
            assert!(Instant::now() < deadline, "timed out waiting for search");
            std::thread::sleep(Duration::from_millis(5));
            vcx.run_until_parked();
        }
        view.read_with(vcx, |view, _| {
            assert_eq!(view.tabs[0].results.matches, vec![1]);
        });

        // Write second chunk and close the pipe (EOF).
        writer.write_all(b"ERROR retry\n").unwrap();
        writer.flush().unwrap();
        drop(writer);

        let deadline = Instant::now() + Duration::from_secs(2);
        while view.read_with(vcx, |view, _| {
            view.tabs[0].log.as_ref().map_or(0, |log| log.len()) < 3
                || view.tabs[0].is_streaming_stdin()
                || view.tabs[0].searching
        }) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for EOF stdin reload"
            );
            std::thread::sleep(Duration::from_millis(5));
            view.update(vcx, |view, cx| view.tick(cx));
            vcx.run_until_parked();
        }

        view.read_with(vcx, |view, _| {
            let tab = &view.tabs[0];
            let log = tab.log.as_ref().unwrap();
            assert_eq!(log.len(), 3);
            assert_eq!(log.line(2, 100).unwrap(), "ERROR retry");
            assert!(tab.bookmarks.contains(&0), "bookmark must survive append");
            assert_eq!(tab.results.matches, vec![1, 2]);
            assert!(!tab.is_streaming_stdin());
            assert_eq!(tab.path_bar_placeholder(), "Standard input");
        });

        // Replacing the stdin tab with a regular file cleans up the temp spool file.
        let dir = tempfile::tempdir().unwrap();
        let real_path = dir.path().join("real.log");
        std::fs::write(&real_path, "regular line\n").unwrap();
        let canonical = real_path.canonicalize().unwrap_or(real_path.clone());

        view.update(vcx, |view, cx| {
            view.replace_active_tab(real_path, cx);
        });
        vcx.run_until_parked();

        view.read_with(vcx, |view, cx| {
            let tab = &view.tabs[0];
            assert!(!tab.is_stdin());
            assert_eq!(tab.path, canonical);
            assert_eq!(
                view.path_input.read(cx).as_str(),
                canonical.display().to_string()
            );
            assert_eq!(view.recent, vec![canonical.clone()]);
        });
        assert!(
            !spool_path.exists(),
            "stdin temp file should be removed once replaced by a regular file"
        );
    }

    #[gpui::test]
    fn stdin_tabs_preserve_existing_session_and_are_excluded_from_saved_sessions(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("session.json");
        let existing_path = dir.path().join("existing.log");
        let real_path = dir.path().join("real.log");
        std::fs::write(&existing_path, "old line\n").unwrap();
        std::fs::write(&real_path, "real line\n").unwrap();

        let mut config = Config::load(None).unwrap();
        config.files.save_session = true;
        config.files.restore_session = true;
        config.files.state_file = Some(state_file.display().to_string());
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });

        // Seed an existing session with `existing.log`.
        let initial_session = Session {
            version: 1,
            active: 0,
            tabs: vec![SavedTab {
                path: existing_path.clone(),
                query: "old".into(),
                options: config.search.clone(),
                bookmarks: BTreeSet::new(),
                selected: 0,
                follow: false,
            }],
            recent: vec![existing_path.clone()],
            results_height: None,
            zoom_percent: None,
        };
        initial_session.save(&state_file).unwrap();

        // Opening only piped stdin takes priority over restore_session and does not clobber session.json.
        let spool = StdinSpool::spawn_reader(std::io::Cursor::new(b"piped\n".to_vec())).unwrap();
        let (stdin_only_view, vcx1) = cx.add_window_view(|window, cx| {
            Workspace::new_with_inputs(config.clone(), vec![InputSource::Stdin(spool)], window, cx)
        });
        vcx1.run_until_parked();
        stdin_only_view.read_with(vcx1, |view, cx| {
            assert_eq!(view.tabs.len(), 1);
            assert!(view.tabs[0].is_stdin());
            view.save_session(cx);
        });
        let after_stdin_only = Session::load(&state_file, &config).unwrap();
        assert_eq!(after_stdin_only.tabs.len(), 1);
        assert_eq!(after_stdin_only.tabs[0].path, existing_path);

        // Opening stdin + a real file excludes stdin from session.json and adjusts `active`.
        let spool2 = StdinSpool::spawn_reader(std::io::Cursor::new(b"piped2\n".to_vec())).unwrap();
        let spool2_path = spool2.path().to_path_buf();
        let (mixed_view, vcx2) = cx.add_window_view(|window, cx| {
            Workspace::new_with_inputs(
                config.clone(),
                vec![
                    InputSource::Stdin(spool2),
                    InputSource::File(real_path.clone()),
                ],
                window,
                cx,
            )
        });
        vcx2.run_until_parked();
        mixed_view.read_with(vcx2, |view, cx| {
            assert_eq!(view.tabs.len(), 2);
            assert!(view.tabs[0].is_stdin());
            assert!(!view.tabs[1].is_stdin());
            assert_eq!(view.active, 1);
            view.save_session(cx);
        });
        let saved_mixed = Session::load(&state_file, &config).unwrap();
        let canonical_real = real_path.canonicalize().unwrap_or(real_path);
        assert_eq!(saved_mixed.tabs.len(), 1);
        assert_eq!(saved_mixed.tabs[0].path, canonical_real);
        assert_eq!(saved_mixed.active, 0);
        assert!(!saved_mixed.recent.contains(&spool2_path));
        assert_eq!(saved_mixed.recent.first(), Some(&canonical_real));
    }

    #[test]
    fn append_regex_alternative_handles_empty_plain_and_escaped_trailing_pipes() {
        assert_eq!(
            append_regex_alternative_with_offset("", "foo"),
            ("foo".into(), 0)
        );
        assert_eq!(
            append_regex_alternative_with_offset("alpha", "beta"),
            ("alpha|beta".into(), 6)
        );
        // Unescaped trailing pipe does not produce `||`.
        assert_eq!(
            append_regex_alternative_with_offset("alpha|", "beta"),
            ("alpha|beta".into(), 6)
        );
        // Escaped literal trailing pipe (`\|`) MUST still get an alternation separator.
        assert_eq!(
            append_regex_alternative_with_offset(r"alpha\|", "beta"),
            (r"alpha\||beta".into(), 8)
        );
        // Escaped backslash followed by real pipe (`\\|`) does not produce `||`.
        assert_eq!(
            append_regex_alternative_with_offset(r"alpha\\|", "beta"),
            (r"alpha\\|beta".into(), 8)
        );
        // Three backslashes + pipe (`\\\|`) is an escaped pipe, so separator is added.
        assert_eq!(
            append_regex_alternative_with_offset(r"alpha\\\|", "beta"),
            (r"alpha\\\||beta".into(), 10)
        );
    }

    #[gpui::test]
    fn appending_presets_in_regex_mode_escapes_literals_runs_search_and_respects_mode_and_ui(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        let literal_special = "[WARN] req(1.2)+$a|b";
        config.search.presets = vec![
            config::Preset {
                name: "Errors".into(),
                pattern: "ERROR|FATAL".into(),
                mode: SearchMode::Regex,
                highlight_match: false,
                select_text: None,
            },
            config::Preset {
                name: "SpecialWarn".into(),
                pattern: literal_special.into(),
                mode: SearchMode::Literal,
                highlight_match: false,
                select_text: None,
            },
            config::Preset {
                name: "DebugTag".into(),
                pattern: "DEBUG".into(),
                mode: SearchMode::Regex,
                highlight_match: true,
                select_text: None,
            },
            config::Preset {
                name: "Empty".into(),
                pattern: String::new(),
                mode: SearchMode::Literal,
                highlight_match: true,
                select_text: None,
            },
        ];
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("append-presets.log");
        std::fs::write(
            &path,
            format!("INFO boot\nERROR fail\n{literal_special}\nDEBUG trace\nINFO a|b literal\n"),
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].options.mode = SearchMode::Literal;
            view.tabs[0].options.highlight_matches = true;
            view.tabs[0].main_selection = LineSelection::single(0);
            view.input.update(cx, |input, cx| input.emplace("INFO", cx));
            cx.notify();
        });
        cx.run_until_parked();

        let (presets_key, append_key) = if cfg!(target_os = "macos") {
            ("cmd-p", "cmd-enter")
        } else {
            ("ctrl-p", "ctrl-enter")
        };

        // 1. In Literal mode: inline `+` buttons are absent, secondary-enter is a no-op,
        //    and secondary-click falls back to normal apply.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        assert!(cx.debug_bounds("preset-append-0").is_none());
        cx.simulate_keystrokes(append_key);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(view.show_presets);
            assert_eq!(view.input.read(cx).as_str(), "INFO");
            assert_eq!(view.tabs[0].options.mode, SearchMode::Literal);
        });

        // 2. Switch to Regex mode with active search "INFO" and highlight_matches = true.
        //    Inline `+` buttons are now rendered, and secondary-enter appends "ERROR|FATAL",
        //    runs the search immediately, closes dropdown, focuses log, and keeps highlight_matches = true.
        view.update_in(cx, |view, window, cx| {
            view.close_presets(window, cx);
            view.tabs[0].options.mode = SearchMode::Regex;
            view.tabs[0].options.highlight_matches = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        assert!(cx.debug_bounds("preset-append-0").is_some());
        assert!(cx.debug_bounds("presets-header-hint").is_some());
        cx.simulate_keystrokes(append_key);
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert_eq!(view.input.read(cx).as_str(), "INFO|ERROR|FATAL");
            assert_eq!(view.tabs[0].query, "INFO|ERROR|FATAL");
            assert_eq!(view.tabs[0].results.matches, vec![0, 1, 4]);
            assert_eq!(view.tabs[0].options.mode, SearchMode::Regex);
            assert!(
                view.tabs[0].options.highlight_matches,
                "appending to non-empty search must preserve existing highlight_matches"
            );
            assert!(view.focus.is_focused(window));
        });

        // 3. Clicking the inline `+` button on a Literal preset escapes regex metacharacters
        //    and appends it without triggering apply_preset.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        let plus_pos = cx
            .debug_bounds("preset-append-1")
            .expect("inline append button for literal preset")
            .center();
        cx.simulate_click(plus_pos, Modifiers::none());
        cx.run_until_parked();
        let escaped_special = regex::escape(literal_special);
        let expected_after_plus = format!("INFO|ERROR|FATAL|{escaped_special}");
        view.read_with(cx, |view, cx| {
            assert!(!view.show_presets);
            assert_eq!(view.input.read(cx).as_str(), expected_after_plus);
            assert_eq!(view.tabs[0].query, expected_after_plus);
            assert_eq!(view.tabs[0].results.matches, vec![0, 1, 2, 4]);
            assert_eq!(view.tabs[0].options.mode, SearchMode::Regex);
            assert!(view.tabs[0].options.highlight_matches);
        });

        // 4. Secondary+Click on a preset row appends it cleanly.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        let item2_bounds = cx.debug_bounds("preset-item-2").expect("preset item 2");
        // Click near the left side of the row (away from the inline `+` button).
        let row_click = point(item2_bounds.origin.x + px(24.), item2_bounds.center().y);
        cx.simulate_click(row_click, Modifiers::secondary_key());
        cx.run_until_parked();
        let expected_after_sec_click = format!("{expected_after_plus}|DEBUG");
        view.read_with(cx, |view, cx| {
            assert!(!view.show_presets);
            assert_eq!(view.input.read(cx).as_str(), expected_after_sec_click);
            assert_eq!(view.tabs[0].query, expected_after_sec_click);
            assert_eq!(view.tabs[0].results.matches, vec![0, 1, 2, 3, 4]);
        });

        // 5. Appending into an empty search box inserts without leading `|`, still escapes
        //    literal presets, keeps Regex mode, and adopts the preset's highlight_match.
        view.update(cx, |view, cx| {
            view.input.update(cx, |input, cx| input.emplace("", cx));
            view.tabs[0].options.highlight_matches = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        cx.simulate_keystrokes(append_key);
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert!(!view.show_presets);
            assert_eq!(view.input.read(cx).as_str(), escaped_special);
            assert_eq!(view.tabs[0].query, escaped_special);
            assert_eq!(view.tabs[0].results.matches, vec![2]);
            assert_eq!(view.tabs[0].options.mode, SearchMode::Regex);
            assert!(
                !view.tabs[0].options.highlight_matches,
                "appending into empty search should adopt preset highlight_match"
            );
        });

        // 6. Escaped trailing pipe (`a\|`) gets a separator (`a\||DEBUG`), whereas unescaped
        //    trailing pipe (`ERROR|`) does not produce `||`.
        view.update(cx, |view, cx| {
            view.input.update(cx, |input, cx| input.emplace(r"a\|", cx));
            cx.notify();
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| view.append_preset(2, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), r"a\||DEBUG");
            assert_eq!(view.tabs[0].results.matches, vec![2, 3, 4]);
        });

        view.update(cx, |view, cx| {
            view.input
                .update(cx, |input, cx| input.emplace("ERROR|", cx));
            cx.notify();
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| view.append_preset(2, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "ERROR|DEBUG");
            assert_eq!(view.tabs[0].results.matches, vec![1, 3]);
        });

        // 7. Empty preset pattern is ignored safely with a warning and does not add trailing `|`.
        view.update_in(cx, |view, window, cx| view.append_preset(3, window, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "ERROR|DEBUG");
            assert!(
                view.status
                    .as_ref()
                    .is_some_and(|s| s.tone == StatusTone::Warning)
            );
        });
    }

    #[gpui::test]
    fn appending_selectable_preset_shifts_and_escapes_selection_and_defers_search(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        config.search.presets = vec![
            config::Preset {
                name: "Port".into(),
                pattern: "port=PORT_NUM".into(),
                mode: SearchMode::Regex,
                highlight_match: false,
                select_text: Some("PORT_NUM".into()),
            },
            config::Preset {
                name: "LiteralHost".into(),
                pattern: "[שלום] host=<HOST.NAME>".into(),
                mode: SearchMode::Literal,
                highlight_match: false,
                select_text: Some("<HOST.NAME>".into()),
            },
            config::Preset {
                name: "BadSelect".into(),
                pattern: "WARN".into(),
                mode: SearchMode::Literal,
                highlight_match: true,
                select_text: Some("MISSING".into()),
            },
        ];
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("append-selectable-presets.log");
        std::fs::write(
            &path,
            "ERROR init\nGET / port=8080\n[שלום] host=api.prod\nWARN disk\n",
        )
        .unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].options.mode = SearchMode::Regex;
            view.tabs[0].options.highlight_matches = true;
            view.input
                .update(cx, |input, cx| input.emplace("ERROR", cx));
            view.start_search(0, cx);
        });
        cx.run_until_parked();

        let selected_range = |input: &Entity<EditableTextState>,
                              window: &mut Window,
                              cx: &mut Context<Workspace>| {
            input
                .update(cx, |state, cx| state.selected_text_range(false, window, cx))
                .expect("selection range")
                .range
        };

        let (presets_key, append_key) = if cfg!(target_os = "macos") {
            ("cmd-p", "cmd-enter")
        } else {
            ("ctrl-p", "ctrl-enter")
        };

        // 1. Append regex preset with `select_text`: shifts selection past "ERROR|",
        //    focuses search box, keeps highlight_matches, and defers running search.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        cx.simulate_keystrokes(append_key);
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            let expected = "ERROR|port=PORT_NUM";
            assert_eq!(view.input.read(cx).as_str(), expected);
            assert_eq!(
                selected_range(&view.input, window, cx),
                utf16_range_for_utf8(expected, 11..19)
            );
            assert_eq!(view.tabs[0].query, "ERROR");
            assert_eq!(view.tabs[0].results.matches, vec![0]);
            assert!(view.tabs[0].options.highlight_matches);
        });

        // Typing replaces only the selected placeholder, and Enter runs the combined search.
        cx.simulate_input("8080");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "ERROR|port=8080");
            assert_eq!(view.tabs[0].query, "ERROR|port=8080");
            assert_eq!(view.tabs[0].results.matches, vec![0, 1]);
        });

        // 2. Append literal preset with Unicode + metacharacters before & inside `select_text`:
        //    escapes literal pattern and selects the exact escaped placeholder range.
        cx.simulate_keystrokes(presets_key);
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        cx.simulate_keystrokes(append_key);
        cx.run_until_parked();
        let expected_combined = r"ERROR|port=8080|\[שלום\] host=<HOST\.NAME>";
        let start = expected_combined.find(r"<HOST\.NAME>").unwrap();
        let end = start + r"<HOST\.NAME>".len();
        view.update_in(cx, |view, window, cx| {
            assert!(!view.show_presets);
            assert!(view.input.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(view.input.read(cx).as_str(), expected_combined);
            assert_eq!(
                selected_range(&view.input, window, cx),
                utf16_range_for_utf8(expected_combined, start..end)
            );
            assert_eq!(view.tabs[0].query, "ERROR|port=8080");
        });

        // Replacing the selected escaped placeholder and pressing Enter matches the Unicode line.
        cx.simulate_input(r"api\.prod");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.tabs[0].query,
                r"ERROR|port=8080|\[שלום\] host=api\.prod"
            );
            assert_eq!(view.tabs[0].results.matches, vec![0, 1, 2]);
        });

        // 3. Selectable preset with missing `select_text` appends, places caret at end, warns, and defers search.
        view.update_in(cx, |view, window, cx| view.append_preset(2, window, cx));
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            let expected = r"ERROR|port=8080|\[שלום\] host=api\.prod|WARN";
            assert_eq!(view.input.read(cx).as_str(), expected);
            let end_utf16 = expected.encode_utf16().count();
            assert_eq!(
                selected_range(&view.input, window, cx),
                end_utf16..end_utf16
            );
            assert!(
                view.status
                    .as_ref()
                    .is_some_and(|s| s.tone == StatusTone::Warning)
            );
            assert_eq!(view.tabs[0].results.matches, vec![0, 1, 2]);
        });
    }

    #[gpui::test]
    fn presets_regex_mode_context_precedence_is_independent_of_binding_order(
        cx: &mut TestAppContext,
    ) {
        let mut config = Config::load(None).unwrap();
        config.files.save_session = false;
        config.files.restore_session = false;
        // Intentionally register `Presets && RegexMode` BEFORE `Presets` for the same key.
        config.keyboard.bindings.push(config::Binding {
            keys: "alt-enter".into(),
            context: config::KeyContext::PresetsRegexMode,
            action: Command::AppendPreset,
        });
        config.keyboard.bindings.push(config::Binding {
            keys: "alt-enter".into(),
            context: config::KeyContext::Presets,
            action: Command::ApplyPreset,
        });
        cx.update(|cx| {
            cx.bind_keys(text_input_default_bindings());
            cx.bind_keys(keybindings(&config).unwrap());
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presets-precedence.log");
        std::fs::write(&path, "INFO boot\nERROR fail\nWARN disk\n").unwrap();
        let log = Arc::new(LogFile::open(&path, &AtomicBool::new(false)).unwrap());
        let (view, cx) =
            cx.add_window_view(|window, cx| Workspace::new(config, vec![path], window, cx));
        view.update(cx, |view, cx| {
            view.tabs[0].log = Some(log);
            view.tabs[0].options.mode = SearchMode::Regex;
            view.input.update(cx, |input, cx| input.emplace("INFO", cx));
            cx.notify();
        });
        cx.run_until_parked();

        // In Regex mode, `Presets && RegexMode` (AppendPreset) wins over `Presets` (ApplyPreset).
        view.update_in(cx, |view, window, cx| view.toggle_presets(window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-enter");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "INFO|error|fatal|panic");
        });

        // In Literal mode, `Presets` (ApplyPreset) wins.
        view.update_in(cx, |view, window, cx| {
            view.tabs[0].options.mode = SearchMode::Literal;
            view.input.update(cx, |input, cx| input.emplace("INFO", cx));
            view.toggle_presets(window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("alt-enter");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            assert_eq!(view.input.read(cx).as_str(), "error|fatal|panic");
        });
    }
}
