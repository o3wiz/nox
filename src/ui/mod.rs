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

struct Tab {
    id: usize,
    path: PathBuf,
    log: Option<Arc<LogFile>>,
    loading: bool,
    searching: bool,
    load_cancel: Arc<AtomicBool>,
    search_cancel: Arc<AtomicBool>,
    revision: usize,
    query: String,
    options: SearchConfig,
    results: SearchResults,
    bookmarks: BTreeSet<usize>,
    selected: usize,
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
    Searched(usize, usize, Result<SearchResults, String>),
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
            load_cancel: Arc::new(AtomicBool::new(false)),
            search_cancel: Arc::new(AtomicBool::new(false)),
            revision: 0,
            query: String::new(),
            options: self.config.search.clone(),
            results: SearchResults::default(),
            bookmarks: BTreeSet::new(),
            selected: 0,
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
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active = 0;
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
        tab.searching = true;
        let (id, revision, query, options, cancel, sender) = (
            tab.id,
            tab.revision,
            tab.query.clone(),
            tab.options.clone(),
            tab.search_cancel.clone(),
            self.sender.clone(),
        );
        let marks = if self.config.viewer.include_bookmarks {
            tab.bookmarks.clone()
        } else {
            BTreeSet::new()
        };
        std::thread::spawn(move || {
            let result = search::search(&log, &query, &options, &marks, &cancel)
                .map_err(|error| error.to_string());
            let _ = sender.send(Message::Searched(id, revision, result));
        });
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
                                let replaced =
                                    tab.log.as_ref().is_some_and(|old| old.replaced_by(&log));
                                if replaced {
                                    tab.bookmarks.clear();
                                }
                                tab.selected = tab.selected.min(log.len().saturating_sub(1));
                                tab.bookmarks.retain(|&line| line < log.len());
                                if tab.log.is_none() && !tab.follow {
                                    tab.scroll
                                        .scroll_to_item(tab.selected, ScrollStrategy::Center);
                                }
                                tab.log = Some(log);
                                tab.error = None;
                                if tab.follow {
                                    tab.scroll.scroll_to_bottom();
                                }
                                self.start_search(index, cx);
                            }
                            Err(error) => {
                                tab.error = Some(error);
                            }
                        }
                    }
                }
                Message::Searched(id, revision, result) => {
                    if let Some(tab) = self
                        .tabs
                        .iter_mut()
                        .find(|tab| tab.id == id && tab.revision == revision)
                    {
                        tab.searching = false;
                        match result {
                            Ok(results) => {
                                tab.results = results;
                                tab.result_scroll.scroll_to_item(0, ScrollStrategy::Top);
                            }
                            Err(error) => {
                                tab.error = Some(error);
                                tab.results = SearchResults::default();
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

    fn command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::ShowFiles => self.show_files = !self.show_files,
            Command::NextLine
            | Command::PreviousLine
            | Command::FirstLine
            | Command::LastLine
            | Command::PageUp
            | Command::PageDown => {
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && let Some(log) = &tab.log
                {
                    let step = ((f32::from(window.viewport_size().height)
                        - self.config.window.results_height
                        - 180.)
                        / self.config.viewer.line_height)
                        .max(1.) as usize;
                    tab.selected = match command {
                        Command::NextLine => tab.selected.saturating_add(1),
                        Command::PreviousLine => tab.selected.saturating_sub(1),
                        Command::FirstLine => 0,
                        Command::LastLine => log.len().saturating_sub(1),
                        Command::PageUp => tab.selected.saturating_sub(step),
                        _ => tab.selected.saturating_add(step),
                    }
                    .min(log.len().saturating_sub(1));
                    tab.follow = false;
                    tab.scroll
                        .scroll_to_item(tab.selected, ScrollStrategy::Center);
                }
            }
            Command::NextBookmark | Command::PreviousBookmark => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
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
                        tab.follow = false;
                        tab.scroll.scroll_to_item(line, ScrollStrategy::Center);
                    }
                }
            }
            Command::Open => self.choose_file(cx),
            Command::Quit => cx.quit(),
            Command::FocusSearch => self.input.read(cx).focus_handle(cx).focus(window, cx),
            Command::FocusLog => {
                self.show_goto = false;
                self.focus.focus(window, cx);
            }
            Command::Search => self.start_search(self.active, cx),
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
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.log.as_ref().is_some_and(|log| !log.is_empty())
                {
                    if !tab.bookmarks.remove(&tab.selected) {
                        tab.bookmarks.insert(tab.selected);
                    }
                    self.start_search(self.active, cx);
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
                        tab.follow = false;
                        tab.scroll.scroll_to_item(target, ScrollStrategy::Center);
                        if let Ok(index) = tab.results.visible.binary_search(&target) {
                            tab.result_scroll
                                .scroll_to_item(index, ScrollStrategy::Center);
                        }
                    }
                }
            }
            Command::CopyLine => {
                if let Some(tab) = self.tabs.get(self.active)
                    && let Some(log) = &tab.log
                {
                    match log.line(tab.selected, usize::MAX) {
                        Ok(text) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            self.status =
                                Some(format!("Copied Ln {}", format_count(tab.selected + 1)));
                        }
                        Err(error) => self.status = Some(error.to_string()),
                    }
                }
            }
            Command::Reload => {
                if !self.tabs.is_empty() {
                    self.load(self.active);
                }
            }
            Command::GoToLine => {
                if self.show_goto {
                    let line = self.goto.read(cx).as_str().trim().parse::<usize>();
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        if let (Ok(line), Some(log)) = (line, &tab.log) {
                            if line > 0 && line <= log.len() {
                                tab.selected = line - 1;
                                tab.follow = false;
                                tab.scroll.scroll_to_item(line - 1, ScrollStrategy::Center);
                                self.show_goto = false;
                                self.status = None;
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
                                tab.follow = false;
                                tab.scroll.scroll_to_item(line, ScrollStrategy::Center);
                            }
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
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Vec::new();
        };
        let Some(log) = tab.log.clone() else {
            return Vec::new();
        };
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
                let selected = tab.selected == line;
                let marked = tab.bookmarks.contains(&line);
                let matched = tab.results.matches.binary_search(&line).is_ok();
                let row_bg = if selected { t.selection } else { t.background };
                let hover_bg = if selected {
                    t.selection
                } else {
                    t.selection_soft
                };

                Some(
                    div()
                        .id((if results { "result" } else { "line" }, line))
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
                        .border_color(if selected {
                            t.accent
                        } else {
                            gpui::transparent_black()
                        })
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
                                .text_color(if marked { t.warning } else { t.accent })
                                .when(marked, |g| g.child(Icon::BookmarkFilled.element(px(12.))))
                                .when(!marked && matched, |g| {
                                    g.child(Icon::MatchDot.element(px(9.)))
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        if let Some(tab) = this.tabs.get_mut(this.active) {
                                            tab.selected = line;
                                        }
                                        this.command(Command::ToggleBookmark, window, cx);
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
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(tab) = this.tabs.get_mut(this.active) {
                                tab.selected = line;
                                tab.follow = false;
                                if results {
                                    tab.scroll.scroll_to_item(line, ScrollStrategy::Center);
                                }
                            }
                            this.focus.focus(window, cx);
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_header(&self, cx: &Context<Self>) -> Stateful<Div> {
        let t = &self.theme;
        let active_tab = self.tabs.get(self.active);
        let has_tabs = !self.tabs.is_empty();
        let follow = active_tab.is_some_and(|tab| tab.follow);
        let is_bookmarked = active_tab.is_some_and(|tab| tab.bookmarks.contains(&tab.selected));

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
                    .child(
                        div()
                            .text_color(if active { t.accent } else { t.muted })
                            .child(Icon::FileText.element(ICON_XS)),
                    )
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
                            .text_color(t.muted)
                            .hover({
                                let hover = t.hover;
                                let fg = t.foreground;
                                move |s| s.bg(hover).text_color(fg)
                            })
                            .child(Icon::Close.element(px(11.)))
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
                    .child(
                        div()
                            .text_color(t.accent)
                            .child(Icon::NoxMark.element(ICON_MD)),
                    )
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
                                    "Remove bookmark"
                                } else {
                                    "Bookmark line"
                                },
                                Command::ToggleBookmark,
                                if is_bookmarked {
                                    ButtonTone::Active
                                } else {
                                    ButtonTone::Ghost
                                },
                                cx,
                            ))
                            .child(self.cmd_icon(
                                "goto",
                                Icon::Hash,
                                "Go to line",
                                Command::GoToLine,
                                if self.show_goto {
                                    ButtonTone::Active
                                } else {
                                    ButtonTone::Ghost
                                },
                                cx,
                            ))
                            .child(self.cmd_icon(
                                "copy",
                                Icon::Copy,
                                "Copy selected line",
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
                            .child(Icon::ClockHistory.element(ICON_XS))
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
                        div()
                            .text_color(if is_fav { t.warning } else { t.muted })
                            .child(
                                if is_fav {
                                    Icon::StarFilled
                                } else {
                                    Icon::FileText
                                }
                                .element(ICON_XS),
                            ),
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
                            .font_family(t.mono_font.clone())
                            .text_size(t.xs_size)
                            .text_color(t.muted)
                            .child(dir),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.open(path.clone(), cx))),
            );
        }

        list
    }

    fn render_goto_bar(&self, cx: &Context<Self>) -> Div {
        let t = &self.theme;
        div()
            .key_context("GoTo")
            .h(px(40.))
            .px_3()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .bg(t.elevated)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .text_color(t.accent)
                    .child(Icon::Hash.element(ICON_SM)),
            )
            .child(
                div()
                    .w(px(180.))
                    .h(px(28.))
                    .px_2()
                    .rounded_md()
                    .bg(t.background)
                    .border_1()
                    .border_color(t.border)
                    .flex()
                    .items_center()
                    .child(
                        text_input("goto-input")
                            .key_context(format!("{DEFAULT_INPUT_CONTEXT} GoTo").as_str())
                            .state(self.goto.downgrade())
                            .placeholder("Line number…")
                            .caret_color(t.accent)
                            .placeholder_color(t.muted)
                            .font_family(t.mono_font.clone())
                            .text_size(t.mono_size)
                            .w_full(),
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
                    .child(
                        div()
                            .text_color(t.danger)
                            .child(Icon::AlertTriangle.element(ICON_SM)),
                    )
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
                            .text_color(t.accent)
                            .child(Icon::NoxMark.element(ICON_HERO)),
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
                    .child(Icon::FolderOpen.element(ICON_MD))
                    .child("Open")
                    .when_some(open_shortcut, |el, key| {
                        el.child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded_sm()
                                .bg(t.on_accent.opacity(0.14))
                                .font_family(t.mono_font.clone())
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
                            div()
                                .text_color(if is_fav { t.warning } else { t.muted })
                                .child(
                                    if is_fav {
                                        Icon::StarFilled
                                    } else {
                                        Icon::ClockHistory
                                    }
                                    .element(ICON_XS),
                                ),
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
                                .font_family(t.mono_font.clone())
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

        let search_box = div()
            .flex_1()
            .min_w(px(280.))
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
            .child(
                div()
                    .text_color(t.muted)
                    .child(Icon::Search.element(ICON_SM)),
            )
            .child(
                div().key_context("Search").flex_1().min_w_0().child(
                    text_input("query")
                        .key_context(format!("{DEFAULT_INPUT_CONTEXT} Search").as_str())
                        .state(self.input.downgrade())
                        .placeholder("Search…")
                        .caret_color(t.accent)
                        .placeholder_color(t.muted)
                        .font(t.mono_font_spec())
                        .text_size(t.mono_size)
                        .w_full(),
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
                    regex_on,
                    t,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(tab) = this.tabs.get_mut(this.active) {
                        tab.options.mode = if tab.options.mode == SearchMode::Regex {
                            SearchMode::Literal
                        } else {
                            SearchMode::Regex
                        };
                        this.start_search(this.active, cx);
                    }
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
                    options.case_sensitive,
                    t,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(tab) = this.tabs.get_mut(this.active) {
                        tab.options.case_sensitive = !tab.options.case_sensitive;
                        this.start_search(this.active, cx);
                    }
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
                    options.whole_word,
                    t,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(tab) = this.tabs.get_mut(this.active) {
                        tab.options.whole_word = !tab.options.whole_word;
                        this.start_search(this.active, cx);
                    }
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
                    options.invert,
                    t,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(tab) = this.tabs.get_mut(this.active) {
                        tab.options.invert = !tab.options.invert;
                        this.start_search(this.active, cx);
                    }
                })),
            );

        let mut bar = div()
            .flex_shrink_0()
            .bg(t.panel)
            .border_t_1()
            .border_color(t.border)
            .px_3()
            .py_2()
            .flex()
            .flex_wrap()
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
                        .text_color(t.muted)
                        .child(Icon::Sparkles.element(ICON_XS))
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

    fn render_status_bar(&self, cx: &Context<Self>) -> Div {
        let t = &self.theme;
        let tab = self.tabs.get(self.active);
        let line_count = tab.and_then(|tab| tab.log.as_ref()).map_or(0, |l| l.len());
        let selected = tab.map_or(0, |tab| if line_count == 0 { 0 } else { tab.selected + 1 });
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
                                .font_family(t.mono_font.clone())
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
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.command(Command::GoToLine, window, cx)
                                })),
                        )
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
                                    .element(px(11.)),
                                )
                                .child(
                                    div()
                                        .font_family(t.mono_font.clone())
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
                                            .font_family(t.mono_font.clone())
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
                        .child(Icon::Check.element(px(12.)))
                        .child(feedback),
                )
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme.clone();
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
            let empty_results = if searching || loading {
                "Searching…"
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

            let source_pane = div()
                .key_context("Log")
                .track_focus(&self.focus)
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
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
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .flex_1()
                                .text_color(t.muted)
                                .child(Icon::Lines.element(px(12.)))
                                .child(
                                    div()
                                        .font_family(t.mono_font.clone())
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
                                .font_family(t.mono_font.clone())
                                .text_size(t.xs_size)
                                .text_color(t.muted)
                                .when(loading, |el| el.text_color(t.accent).child("Indexing…"))
                                .child(format_count(line_count)),
                        ),
                )
                .when(line_count == 0, |pane| {
                    pane.child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(t.small_size)
                            .text_color(t.muted)
                            .child(if loading {
                                "Opening file…"
                            } else if tab_error.is_some() {
                                "Cannot read file"
                            } else {
                                "Empty file"
                            }),
                    )
                })
                .when(line_count > 0, |pane| {
                    pane.child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(
                                uniform_list(
                                    "source",
                                    line_count,
                                    cx.processor(|this, range, _, cx| this.rows(range, false, cx)),
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
                            }),
                    )
                });

            let results_header = div()
                .h(px(30.))
                .px_3()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_between()
                .bg(t.panel)
                .border_y_1()
                .border_color(t.border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(t.xs_size)
                        .text_color(t.muted)
                        .child(
                            div()
                                .text_color(if match_count > 0 { t.accent } else { t.muted })
                                .child(Icon::ListFilter.element(ICON_XS)),
                        )
                        .child(
                            div()
                                .font_family(t.mono_font.clone())
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
                                    .text_color(t.warning)
                                    .child(Icon::AlertTriangle.element(ICON_XS))
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
                        .when(searching, |el| el.text_color(t.accent).child("Searching…")),
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

            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(source_pane)
                .child(self.render_search_bar(&options, cx))
                .child(results_header)
                .when(result_count == 0, |col| {
                    col.child(
                        div()
                            .h(px(72.))
                            .flex_shrink_0()
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
                            cx.processor(|this, range, _, cx| this.rows(range, true, cx)),
                        )
                        .with_horizontal_sizing_behavior(
                            ListHorizontalSizingBehavior::Unconstrained,
                        )
                        .with_width_from_item(Some(widest_result))
                        .track_scroll(&result_scroll)
                        .h(px(self.config.window.results_height))
                        .max_h(relative(0.45))
                        .flex_shrink_0(),
                    )
                })
        };

        div()
            .size_full()
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
            .child(self.render_header(cx))
            .when(self.show_files, |root| {
                root.child(self.render_file_drawer(cx))
            })
            .when(self.show_goto, |root| root.child(self.render_goto_bar(cx)))
            .when_some(tab_error, |root, error| {
                root.child(self.render_error_banner(error, cx))
            })
            .child(main_area)
            .when(self.config.window.show_status_bar, |root| {
                root.child(self.render_status_bar(cx))
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
    fn all_icons_parse_as_valid_svgs(cx: &mut TestAppContext) {
        cx.update(|cx| {
            for icon in Icon::ALL {
                assert!(
                    cx.svg_renderer().parse_svg(icon.svg_bytes()).is_ok(),
                    "Icon::{icon:?} failed SVG parsing"
                );
            }
        });
    }

    #[test]
    fn resolves_preferred_fonts_and_formats_telemetry() {
        let installed = vec![
            "Menlo".to_owned(),
            ".SF NS Mono".to_owned(),
            "SF Pro Text".to_owned(),
        ];
        assert_eq!(
            theme::resolve_font("monospace", &["JetBrains Mono", "Menlo"], &installed),
            "Menlo"
        );
        assert_eq!(
            theme::resolve_font("SF Mono", &["JetBrains Mono", "Menlo"], &installed),
            ".SF NS Mono"
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
}
