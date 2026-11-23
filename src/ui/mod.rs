mod components;
mod icons;
mod theme;

use components::{
    ButtonTone, TOOLTIP_DELAY, Tooltip, format_bytes, format_count, icon_button, search_toggle,
    toolbar_divider,
};
use gpui::{prelude::*, *};
use gpui_ce_elements::editable_text::{
    EditableTextState, StringStorage,
    actions::{DEFAULT_INPUT_CONTEXT, default_bindings},
    text_input,
};
use icons::Icon;
use nox::{
    config::{self, Command, Config, SearchConfig, SearchMode},
    logfile::{LogFile, Stamp},
    search::{self, SearchResults},
    selection::LineSelection,
    session::{self, SavedTab, Session},
};
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
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
    config
        .keyboard
        .bindings
        .iter()
        .map(|binding| {
            Ok(KeyBinding::load(
                &binding.keys,
                Box::new(Execute {
                    command: binding.action,
                }),
                Some(KeyBindingContextPredicate::parse(binding.context.as_str())?.into()),
                false,
                None,
                &DummyKeyboardMapper,
            )?)
        })
        .collect()
}

const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BULK_BOOKMARK_LINES: usize = 100_000;
const SEARCH_INDICATOR_DELAY: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Main,
    Results,
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
    results: SearchResults,
    bookmarks: BTreeSet<usize>,
    selected: usize,
    main_selection: LineSelection,
    result_selection: LineSelection,
    follow: bool,
    scroll: UniformListScrollHandle,
    result_scroll: UniformListScrollHandle,
    error: Option<String>,
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

pub struct Workspace {
    config: Config,
    theme: Theme,
    highlights: Vec<(Regex, u32)>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    input: Entity<EditableTextState>,
    goto: Entity<EditableTextState>,
    show_goto: bool,
    focus: FocusHandle,
    results_focus: FocusHandle,
    active_pane: Pane,
    drag_selecting: Option<Pane>,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    last_poll: Instant,
    status: Option<String>,
    _timer: Task<()>,
    _subscriptions: Vec<Subscription>,
    state_path: Option<PathBuf>,
    recent: Vec<PathBuf>,
    show_files: bool,
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
        let subscriptions = vec![
            cx.on_app_quit(|this, cx| {
                this.save_session(cx);
                async {}
            }),
            cx.on_release(|this, cx| this.save_session(cx)),
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
            goto: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            show_goto: false,
            focus,
            results_focus,
            active_pane: Pane::Main,
            drag_selecting: None,
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
        };
        for path in paths {
            this.open(path, cx);
        }
        if restore {
            for (tab, saved) in this.tabs.iter_mut().zip(saved.tabs) {
                tab.query = saved.query;
                tab.options = saved.options;
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
        if let Some(error) = session_error {
            this.status = Some(format!("{error} · session saving disabled"));
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
            results: SearchResults::default(),
            bookmarks: BTreeSet::new(),
            selected: 0,
            main_selection: LineSelection::single(0),
            result_selection: LineSelection::empty(),
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
        self.drag_selecting = None;
        let query = self
            .tabs
            .get(index)
            .map(|tab| tab.query.clone())
            .unwrap_or_default();
        self.input.update(cx, |input, cx| input.emplace(&query, cx));
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
        self.drag_selecting = None;
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
                tab.results = results;
            } else {
                let len = results.visible.len();
                tab.results = results;
                tab.result_selection = LineSelection::single_clamped(0, len);
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
                    tab.results = results;
                } else {
                    let len = results.visible.len();
                    tab.results = results;
                    tab.result_selection = LineSelection::single_clamped(0, len);
                    tab.result_scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
            }
            Err(error) => {
                tab.error = Some(error);
                tab.results = SearchResults::default();
                tab.result_selection = LineSelection::empty();
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
                                } else if initial || tab.main_selection.is_empty() {
                                    tab.main_selection =
                                        LineSelection::single_clamped(tab.selected, log.len());
                                } else {
                                    tab.main_selection.clamp(log.len());
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
                        Ok(()) => "Exported".into(),
                        Err(error) => format!("Export failed: {error}"),
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
                    this.status = Some(format!("File dialog failed: {other:?}"));
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
                    this.status = Some(format!("Save dialog failed: {other:?}"));
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
        match pane {
            Pane::Main => self.focus.focus(window, cx),
            Pane::Results => self.results_focus.focus(window, cx),
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
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
                let step = ((f32::from(window.viewport_size().height)
                    - self.config.window.results_height
                    - 180.)
                    / self.config.viewer.line_height)
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
                let pane_height = self
                    .config
                    .window
                    .results_height
                    .min(f32::from(window.viewport_size().height) * 0.45);
                let step = (pane_height / self.config.viewer.line_height).max(1.) as usize;
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
            Command::ShowFiles => self.show_files = !self.show_files,
            Command::NextLine
            | Command::PreviousLine
            | Command::FirstLine
            | Command::LastLine
            | Command::PageUp
            | Command::PageDown => {
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
            Command::ClearSelection => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                if let Some(tab) = self.tabs.get_mut(self.active) {
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
            Command::SwitchPane => {
                let current = self.active_pane_for(window);
                match current {
                    Pane::Main => {
                        let has_results = self
                            .tabs
                            .get(self.active)
                            .is_some_and(|tab| !tab.results.visible.is_empty());
                        if has_results {
                            self.focus_pane(Pane::Results, window, cx);
                            return;
                        }
                    }
                    Pane::Results => {
                        self.focus_pane(Pane::Main, window, cx);
                        return;
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
                            tab.follow = false;
                            tab.scroll.scroll_to_item(line, ScrollStrategy::Center);
                            if let Ok(index) = tab.results.visible.binary_search(&line) {
                                tab.result_selection.select_single(index);
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
                self.input.read(cx).focus_handle(cx).focus(window, cx);
            }
            Command::FocusLog => {
                self.focus_pane(Pane::Main, window, cx);
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
            Command::CloseTab => {
                let index = self.active;
                self.close_tab_at(index, cx);
            }
            Command::NextTab => {
                if !self.tabs.is_empty() {
                    self.switch((self.active + 1) % self.tabs.len(), cx);
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
                        self.status = Some(format!(
                            "Too many lines selected to bookmark (limit {})",
                            format_count(MAX_BULK_BOOKMARK_LINES)
                        ));
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
                        tab.follow = false;
                        tab.scroll.scroll_to_item(target, ScrollStrategy::Center);
                        if let Ok(index) = tab.results.visible.binary_search(&target) {
                            tab.result_selection.select_single(index);
                            tab.result_scroll
                                .scroll_to_item(index, ScrollStrategy::Center);
                        }
                    }
                }
            }
            Command::CopyLine => {
                let pane = self.active_pane_for(window);
                self.active_pane = pane;
                if let Some(tab) = self.tabs.get(self.active)
                    && let Some(log) = &tab.log
                {
                    let count = Self::selection_count(tab, pane);
                    if count == 0 {
                        self.status = Some("No lines selected".into());
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
                            self.status = Some("Selection too large to copy (limit 64 MB)".into());
                        } else if lines_to_copy.is_empty() {
                            self.status = Some("No lines selected".into());
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
                                self.status = Some(error);
                            } else {
                                cx.write_to_clipboard(ClipboardItem::new_string(copied));
                                self.status = Some(if lines_to_copy.len() == 1 {
                                    format!("Copied Ln {}", format_count(lines_to_copy[0] + 1))
                                } else {
                                    format!("Copied {} lines", format_count(lines_to_copy.len()))
                                });
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
                if self.tabs.is_empty() {
                    self.show_goto = false;
                } else if self.show_goto {
                    let line = self.goto.read(cx).as_str().trim().parse::<usize>();
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        if let (Ok(line), Some(log)) = (line, &tab.log) {
                            if line > 0 && line <= log.len() {
                                tab.selected = line - 1;
                                tab.main_selection.select_single(line - 1);
                                tab.follow = false;
                                tab.scroll.scroll_to_item(line - 1, ScrollStrategy::Center);
                                self.show_goto = false;
                                self.status = None;
                                self.active_pane = Pane::Main;
                                self.focus.focus(window, cx);
                            } else {
                                self.status = Some(format!("Range: 1–{}", format_count(log.len())));
                            }
                        } else {
                            self.status = Some("Invalid line number".into());
                        }
                    }
                } else {
                    self.show_goto = true;
                    self.goto.update(cx, |input, cx| input.emplace("", cx));
                    self.goto.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
            Command::ExportResults => self.export(cx),
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

    fn overview(&self, cx: &Context<Self>) -> Div {
        let t = &self.theme;
        let mut overview = div()
            .relative()
            .h_full()
            .w(px(self.config.viewer.overview_width))
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
                overview = overview.child(
                    div()
                        .id(("overview", bucket))
                        .absolute()
                        .top(relative(bucket as f32 / 256.))
                        .left(px(2.))
                        .right(px(2.))
                        .h(px(3.))
                        .rounded_sm()
                        .bg(color)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(tab) = this.tabs.get_mut(this.active) {
                                tab.selected = line;
                                tab.main_selection.select_single(line);
                                tab.follow = false;
                                tab.scroll.scroll_to_item(line, ScrollStrategy::Center);
                            }
                            this.active_pane = Pane::Main;
                            this.show_goto = false;
                            this.focus.focus(window, cx);
                            cx.notify();
                        })),
                );
            }
        }
        overview
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
        range
            .filter_map(|row| {
                let line = if results {
                    *tab.results.visible.get(row)?
                } else {
                    row
                };
                let text = log
                    .line(line, self.config.viewer.max_display_chars.saturating_mul(4))
                    .unwrap_or_else(|e| format!("Read error: {e}"));
                let mut chars = text.chars();
                let mut text: String = chars
                    .by_ref()
                    .take(self.config.viewer.max_display_chars)
                    .collect();
                if chars.next().is_some() {
                    text.push_str(" … [truncated]");
                }
                let text = text.replace('\t', &" ".repeat(self.config.viewer.tab_width));
                let text_color: Hsla = self
                    .highlights
                    .iter()
                    .find(|(pattern, _)| pattern.is_match(&text))
                    .map(|(_, color)| rgb_to_hsla(rgb(*color)))
                    .unwrap_or(t.foreground);
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
                        .h(px(self.config.viewer.line_height))
                        .min_w_full()
                        .whitespace_nowrap()
                        .font(t.mono_font_spec())
                        .text_size(t.mono_size)
                        .bg(row_bg)
                        .hover(move |s| s.bg(hover_bg))
                        .border_l_2()
                        .border_color(border_color)
                        .text_color(text_color)
                        .cursor_pointer()
                        .child(
                            div()
                                .id((if results { "rmark" } else { "mark" }, line))
                                .w(px(28.))
                                .h_full()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .when(marked, |g| {
                                    g.child(Icon::BookmarkFilled.element(px(12.), t.warning))
                                })
                                .when(!marked && matched, |g| {
                                    g.child(Icon::MatchDot.element(px(9.), t.accent))
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
                                    .w(px(72.))
                                    .h_full()
                                    .pr_3()
                                    .mr_3()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .border_r_1()
                                    .border_color(t.border.opacity(0.6))
                                    .text_size(t.small_size)
                                    .text_color(if selected { t.foreground } else { t.muted })
                                    .child(format!("{}", line + 1)),
                            )
                        })
                        .child(div().pr_4().child(text))
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

    fn render_header(&self, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let t = &self.theme;
        let pane = self.active_pane_for(window);
        let active_tab = self.tabs.get(self.active);
        let has_tabs = !self.tabs.is_empty();
        let follow = active_tab.is_some_and(|tab| tab.follow);
        let selected_count = active_tab.map_or(0, |tab| Self::selection_count(tab, pane));
        let is_bookmarked = active_tab.is_some_and(|tab| Self::selection_all_bookmarked(tab, pane));

        let mut tab_strip = div()
            .id("tabs")
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
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_tab_at(index, cx);
                            })),
                    )
                    .tooltip_show_delay(Duration::from_millis(450))
                    .tooltip(move |_window, cx| {
                        Tooltip::view(full_path.clone(), None, tip_theme.clone(), cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.switch(index, cx))),
            );
        }

        div()
            .id("header")
            .h(px(46.))
            .px_3()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_3()
            .bg(t.panel)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
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
                    .child(self.cmd_icon(
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
                    ))
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
                            .child(self.cmd_icon(
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
                            ))
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
                                if selected_count > 1 {
                                    format!("Copy {} selected lines", format_count(selected_count))
                                } else {
                                    "Copy selected line".to_owned()
                                },
                                Command::CopyLine,
                                ButtonTone::Ghost,
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

    fn render_goto_popover(&self, line_count: usize, cx: &Context<Self>) -> Stateful<Div> {
        let t = &self.theme;
        let right_inset = if self.config.viewer.show_overview && line_count > 0 {
            self.config.viewer.overview_width + 12.
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

    fn render_search_bar(&self, options: &SearchConfig, cx: &Context<Self>) -> Div {
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
                        if regex_on {
                            "Regular expression (on)"
                        } else {
                            "Regular expression (off)"
                        },
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
                        if options.case_sensitive {
                            "Match case (on)"
                        } else {
                            "Match case (off)"
                        },
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
                        if options.whole_word {
                            "Whole words (on)"
                        } else {
                            "Whole words (off)"
                        },
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
                        if options.invert {
                            "Exclude matches (on)"
                        } else {
                            "Exclude matches (off)"
                        },
                        t.shortcut(Command::ToggleInvert),
                        options.invert,
                        t,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.command(Command::ToggleInvert, window, cx)
                    })),
                );

        let mut bar = div()
            .h(px(48.))
            .flex_shrink_0()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.border)
            .px_3()
            .flex()
            .items_center()
            .gap_2()
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
            let mut presets = div()
                .id("presets")
                .flex()
                .items_center()
                .gap_1p5()
                .flex_shrink_0()
                .child(toolbar_divider(t))
                .child(
                    div()
                        .id("presets-icon")
                        .child(Icon::Sparkles.element(ICON_XS, t.muted))
                        .tooltip_show_delay(TOOLTIP_DELAY)
                        .tooltip({
                            let tip_theme = t.clone();
                            move |_, cx| {
                                Tooltip::view("Saved searches", None, tip_theme.clone(), cx)
                            }
                        }),
                );

            for (index, preset) in self.config.search.presets.iter().enumerate() {
                let active = current_input == preset.pattern && options.mode == preset.mode;
                let preset_clone = preset.clone();
                let tip_pattern = SharedString::from(preset.pattern.clone());
                let tip_theme = t.clone();
                let hover = t.hover;
                let fg = t.foreground;

                presets = presets.child(
                    div()
                        .id(("preset", index))
                        .h(px(26.))
                        .px_2p5()
                        .rounded_md()
                        .border_1()
                        .border_color(if active { t.accent_border } else { t.border })
                        .bg(if active {
                            t.accent_subtle
                        } else {
                            t.background
                        })
                        .text_color(if active { t.accent } else { t.muted })
                        .text_size(t.xs_size)
                        .font_weight(FontWeight::MEDIUM)
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .hover(move |s| {
                            if active {
                                s
                            } else {
                                s.bg(hover).text_color(fg)
                            }
                        })
                        .child(preset.name.clone())
                        .tooltip_show_delay(TOOLTIP_DELAY)
                        .tooltip(move |_, cx| {
                            Tooltip::view(tip_pattern.clone(), None, tip_theme.clone(), cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.input
                                .update(cx, |input, cx| input.emplace(&preset_clone.pattern, cx));
                            if let Some(tab) = this.tabs.get_mut(this.active) {
                                tab.options.mode = preset_clone.mode;
                            }
                            this.start_search(this.active, cx);
                        })),
                );
            }
            bar = bar.child(presets);
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
                    .when(tab.is_none(), |left| left.child("Ready")),
            )
            .when_some(self.status.clone(), |bar, feedback| {
                bar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_color(t.accent)
                        .child(Icon::Check.element(px(12.), t.accent))
                        .child(feedback),
                )
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let file_path = tab.map(|tab| tab.path.display().to_string());

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

            let source_log_area = div()
                .key_context("Log")
                .track_focus(&self.focus)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.focus_pane(Pane::Main, window, cx)),
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
                        .flex_1()
                        .min_w_0()
                        .h_full(),
                    )
                    .when(self.config.viewer.show_overview, |row| {
                        row.child(self.overview(cx))
                    })
                });

            let source_pane = div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .debug_selector(|| "file-path-bar".into())
                        .h(px(26.))
                        .px_3()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .bg(t.background)
                        .border_b_1()
                        .border_color(t.border.opacity(0.6))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.focus_pane(Pane::Main, window, cx)
                            }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .flex_1()
                                .text_color(if main_focused { t.foreground } else { t.muted })
                                .child(div().w(px(2.)).h(px(12.)).rounded_full().bg(
                                    if main_focused {
                                        t.accent
                                    } else {
                                        gpui::transparent_black()
                                    },
                                ))
                                .child(Icon::Lines.element(
                                    px(12.),
                                    if main_focused { t.accent } else { t.muted },
                                ))
                                .child(
                                    div()
                                        .font(t.mono_font_spec())
                                        .text_size(t.xs_size)
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(file_path.unwrap_or_default()),
                                ),
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
                                .when(loading, |el| el.text_color(t.accent).child("Indexing…"))
                                .child(format_count(line_count)),
                        ),
                )
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(source_log_area)
                        .when(self.show_goto, |body| {
                            body.child(self.render_goto_popover(line_count, cx))
                        }),
                );

            let results_header =
                div()
                    .h(px(30.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(t.panel)
                    .border_y_1()
                    .border_color(t.border)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.focus_pane(Pane::Results, window, cx)
                        }),
                    )
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
                    .when(result_count > 0, |header| {
                        header.child(
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
                    });

            let results_body = div()
                .key_context("Log")
                .track_focus(&self.results_focus)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.focus_pane(Pane::Results, window, cx)),
                )
                .h(px(self.config.window.results_height))
                .max_h(relative(0.45))
                .flex_shrink_0()
                .w_full()
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
                        uniform_list(
                            "results",
                            result_count,
                            cx.processor(|this, range, window, cx| {
                                this.rows(range, true, window, cx)
                            }),
                        )
                        .with_horizontal_sizing_behavior(
                            ListHorizontalSizingBehavior::Unconstrained,
                        )
                        .with_width_from_item(Some(widest_result))
                        .track_scroll(&result_scroll)
                        .flex_1()
                        .w_full()
                        .min_h_0(),
                    )
                });

            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(source_pane)
                .child(self.render_search_bar(&options, cx))
                .child(results_header)
                .child(results_body)
        };

        div()
            .size_full()
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.drag_selecting = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.drag_selecting = None;
                }),
            )
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                for path in paths.paths() {
                    this.open(path.clone(), cx);
                }
            }))
            .flex()
            .flex_col()
            .key_context("Workspace")
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
                            ..Default::default()
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
}
