use gpui::{prelude::*, *};
use gpui_ce_elements::editable_text::{
    EditableTextState, StringStorage,
    actions::{DEFAULT_INPUT_CONTEXT, default_bindings},
    text_input,
};
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
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

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
#[derive(Clone, Copy)]
struct Palette {
    background: u32,
    panel: u32,
    foreground: u32,
    muted: u32,
    accent: u32,
    selection: u32,
    border: u32,
}
impl Palette {
    fn new(config: &Config) -> Self {
        let a = &config.appearance;
        Self {
            background: config::color(&a.background).unwrap(),
            panel: config::color(&a.panel).unwrap(),
            foreground: config::color(&a.foreground).unwrap(),
            muted: config::color(&a.muted).unwrap(),
            accent: config::color(&a.accent).unwrap(),
            selection: config::color(&a.selection).unwrap(),
            border: config::color(&a.border).unwrap(),
        }
    }
}
pub struct Workspace {
    config: Config,
    palette: Palette,
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
    status: String,
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
        let mut this = Self {
            palette: Palette::new(&config),
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
            status: "Ready · Open a log file to begin".into(),
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
            this.status = format!("{error} · session saving disabled to preserve the file");
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
                                self.status = format!(
                                    "{} bytes · UTF-8 · watching for changes",
                                    log.stamp.len
                                );
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
                    self.status = match result {
                        Ok(()) => "Results exported".into(),
                        Err(error) => format!("Export failed: {error}"),
                    }
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
                    this.status = format!("File dialog failed: {other:?}");
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
            log.path.parent().unwrap_or(std::path::Path::new(".")),
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
                    this.status = format!("Save dialog failed: {other:?}");
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
                        - 220.)
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
                if !self.tabs.is_empty() {
                    self.tabs.remove(self.active);
                    self.active = self.active.min(self.tabs.len().saturating_sub(1));
                    let query = self
                        .tabs
                        .get(self.active)
                        .map(|tab| tab.query.clone())
                        .unwrap_or_default();
                    self.input.update(cx, |input, cx| input.emplace(&query, cx));
                }
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
                            self.status = format!("Copied line {}", tab.selected + 1);
                        }
                        Err(error) => self.status = error.to_string(),
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
                                self.focus.focus(window, cx);
                            } else {
                                self.status = format!("Line must be between 1 and {}", log.len());
                            }
                        } else {
                            self.status = "Enter a valid line number".into();
                        }
                    }
                } else {
                    self.show_goto = true;
                    self.goto.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
            Command::ExportResults => self.export(cx),
        }
        cx.notify();
    }
    fn button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        command: Command,
        active: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let p = self.palette;
        div()
            .id(id)
            .whitespace_nowrap()
            .flex_shrink_0()
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .bg(rgb(if active { p.selection } else { p.panel }))
            .text_color(rgb(if active { p.accent } else { p.foreground }))
            .hover(move |s| s.bg(rgb(p.selection)))
            .child(label.into())
            .on_click(cx.listener(move |this, _, window, cx| this.command(command, window, cx)))
    }
    fn overview(&self, cx: &Context<Self>) -> Div {
        let mut overview = div()
            .relative()
            .h_full()
            .w(px(self.config.viewer.overview_width))
            .flex_shrink_0()
            .bg(rgb(self.palette.panel));
        if let Some(tab) = self.tabs.get(self.active)
            && let Some(log) = &tab.log
        {
            let mut markers = std::collections::BTreeMap::new();
            for &line in &tab.results.matches {
                markers.insert(
                    line.saturating_mul(256) / log.len().max(1),
                    (line, self.palette.accent),
                );
            }
            for &line in &tab.bookmarks {
                markers.insert(
                    line.saturating_mul(256) / log.len().max(1),
                    (line, self.palette.foreground),
                );
            }
            for (bucket, (line, color)) in markers {
                overview = overview.child(
                    div()
                        .id(("overview", bucket))
                        .absolute()
                        .top(relative(bucket as f32 / 256.))
                        .h(px(3.))
                        .w_full()
                        .bg(rgb(color))
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
        let p = self.palette;
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
                    text.push_str(" … [display truncated]");
                }
                let text = text.replace('\t', &" ".repeat(self.config.viewer.tab_width));
                let color = self
                    .highlights
                    .iter()
                    .find(|(pattern, _)| pattern.is_match(&text))
                    .map(|(_, color)| *color)
                    .unwrap_or(p.foreground);
                let marked = tab.bookmarks.contains(&line);
                let matched = tab.results.matches.binary_search(&line).is_ok();
                Some(
                    div()
                        .id((if results { "result" } else { "line" }, line))
                        .flex()
                        .h(px(self.config.viewer.line_height))
                        .min_w_full()
                        .whitespace_nowrap()
                        .bg(rgb(if tab.selected == line {
                            p.selection
                        } else {
                            p.background
                        }))
                        .text_color(rgb(color))
                        .cursor_pointer()
                        .child(
                            div()
                                .w(px(28.))
                                .flex_shrink_0()
                                .text_color(rgb(p.accent))
                                .child(if marked {
                                    "◆"
                                } else if matched {
                                    "•"
                                } else {
                                    " "
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
                                    .w(px(86.))
                                    .flex_shrink_0()
                                    .text_color(rgb(p.muted))
                                    .child(format!("{:>8}  ", line + 1)),
                            )
                        })
                        .child(text)
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
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let tab = self.tabs.get(self.active);
        let widest_line = tab
            .and_then(|t| t.log.as_ref())
            .map_or(0, |log| log.widest_line);
        let widest_result = tab.map_or(0, |t| t.results.widest_row);
        let line_count = tab.and_then(|t| t.log.as_ref()).map_or(0, |log| log.len());
        let result_count = tab.map_or(0, |t| t.results.visible.len());
        let follow = tab.is_some_and(|t| t.follow);
        let options = tab
            .map(|t| t.options.clone())
            .unwrap_or_else(|| self.config.search.clone());
        let scroll = tab.map(|t| t.scroll.clone()).unwrap_or_default();
        let result_scroll = tab.map(|t| t.result_scroll.clone()).unwrap_or_default();
        let path = tab
            .map(|t| t.path.display().to_string())
            .unwrap_or_else(|| "A clear view into your logs".into());
        let message = tab.and_then(|t| t.error.clone()).unwrap_or_else(|| {
            if let Some(tab) = tab {
                format!(
                    "Line {} / {} · {} bookmarks · {}",
                    if line_count == 0 { 0 } else { tab.selected + 1 },
                    line_count,
                    tab.bookmarks.len(),
                    self.status
                )
            } else {
                self.status.clone()
            }
        });
        let result_label = tab
            .map(|t| {
                format!(
                    "FILTERED RESULTS   {} matches · {} rows{}{}",
                    t.results.matches.len(),
                    result_count,
                    if t.results.limited {
                        " · result limit reached"
                    } else {
                        ""
                    },
                    if t.searching { " · searching…" } else { "" }
                )
            })
            .unwrap_or_else(|| "FILTERED RESULTS".into());
        let file_label = format!(
            "SOURCE   {line_count} lines{}",
            if tab.is_some_and(|t| t.loading) {
                " · indexing…"
            } else {
                ""
            }
        );
        let mut tabs = div()
            .id("tabs")
            .flex()
            .gap_1()
            .px_3()
            .py_2()
            .bg(rgb(p.panel))
            .overflow_x_scroll();
        for (index, tab) in self.tabs.iter().enumerate() {
            let label = tab
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            tabs = tabs.child(
                div()
                    .id(("tab", tab.id))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if index == self.active {
                        p.selection
                    } else {
                        p.panel
                    }))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.switch(index, cx))),
            );
        }
        let mut controls = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(p.panel))
            .child(
                div().key_context("Search").flex_1().min_w_0().child(
                    text_input("query")
                        .key_context(format!("{DEFAULT_INPUT_CONTEXT} Search").as_str())
                        .state(self.input.downgrade())
                        .placeholder("Search the log…  Enter to apply")
                        .caret_color(rgb_to_hsla(rgb(p.accent)))
                        .placeholder_color(rgb(p.muted))
                        .px_3()
                        .py_2()
                        .w_full()
                        .rounded_md()
                        .bg(rgb(p.background)),
                ),
            )
            .child(self.button("search", "Search", Command::Search, true, cx));
        for (id, label, enabled) in [
            ("regex", ".*", options.mode == SearchMode::Regex),
            ("case", "Aa", options.case_sensitive),
            ("word", "Word", options.whole_word),
            ("invert", "Not", options.invert),
        ] {
            controls = controls.child(
                div()
                    .id(id)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(if enabled { p.accent } else { p.muted }))
                    .bg(rgb(if enabled { p.selection } else { p.panel }))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tab) = this.tabs.get_mut(this.active) {
                            match id {
                                "regex" => {
                                    tab.options.mode = if tab.options.mode == SearchMode::Regex {
                                        SearchMode::Literal
                                    } else {
                                        SearchMode::Regex
                                    }
                                }
                                "case" => tab.options.case_sensitive = !tab.options.case_sensitive,
                                "word" => tab.options.whole_word = !tab.options.whole_word,
                                _ => tab.options.invert = !tab.options.invert,
                            }
                            this.start_search(this.active, cx);
                        }
                    })),
            );
        }
        let mut presets = div()
            .id("presets")
            .flex()
            .gap_2()
            .items_center()
            .px_3()
            .py_1()
            .overflow_x_scroll()
            .text_color(rgb(p.muted))
            .child("PRESETS");
        for (index, preset) in self.config.search.presets.iter().enumerate() {
            let preset = preset.clone();
            presets = presets.child(
                div()
                    .id(("preset", index))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .child(preset.name)
                    .hover(move |s| s.text_color(rgb(p.accent)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.input
                            .update(cx, |input, cx| input.emplace(&preset.pattern, cx));
                        if let Some(tab) = this.tabs.get_mut(this.active) {
                            tab.options.mode = preset.mode;
                        }
                        this.start_search(this.active, cx);
                    })),
            );
        }
        let mut content = div().flex_1().min_h_0().flex().flex_col().child(
            div()
                .key_context("Log")
                .track_focus(&self.focus)
                .flex()
                .justify_between()
                .px_3()
                .py_1()
                .text_color(rgb(p.muted))
                .child(file_label)
                .child("Click gutter to bookmark · F3 next match"),
        );
        if self.tabs.is_empty() {
            let mut welcome = div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_4()
                .child(div().text_3xl().text_color(rgb(p.accent)).child("NOX"))
                .child("Explore. Filter. Follow.")
                .child(
                    "Open a log file with the Open button or pass file paths on the command line.",
                )
                .child(self.button("welcome-open", "Open log files", Command::Open, true, cx));
            for (index, path) in self.config.files.favorites.iter().enumerate() {
                let path = path.clone();
                welcome = welcome.child(
                    div()
                        .id(("favorite", index))
                        .cursor_pointer()
                        .text_color(rgb(p.accent))
                        .child(path.clone())
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.open(PathBuf::from(&path), cx)),
                        ),
                );
            }
            content = content.child(welcome);
        } else {
            content = content.child(
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
            );
        }
        content = content
            .child(controls)
            .child(presets)
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .bg(rgb(p.panel))
                    .text_color(rgb(p.muted))
                    .child(result_label)
                    .child(self.button("export", "Export", Command::ExportResults, false, cx)),
            )
            .child(
                uniform_list(
                    "results",
                    result_count,
                    cx.processor(|this, range, _, cx| this.rows(range, true, cx)),
                )
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(Some(widest_result))
                .track_scroll(&result_scroll)
                .h(px(self.config.window.results_height))
                .max_h(relative(0.45))
                .flex_shrink_0(),
            );
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
            .bg(rgb(p.background))
            .text_color(rgb(p.foreground))
            .font_family(self.config.appearance.font_family.clone())
            .text_size(px(self.config.appearance.font_size))
            .on_action(cx.listener(|this, action: &Execute, window, cx| {
                this.command(action.command, window, cx)
            }))
            .child(
                div()
                    .id("toolbar")
                    .overflow_x_scroll()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(rgb(p.border))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(p.accent))
                            .mr_3()
                            .child("NOX"),
                    )
                    .child(self.button("open", "Open", Command::Open, false, cx))
                    .child(self.button("library", "Files", Command::ShowFiles, self.show_files, cx))
                    .child(self.button("reload", "Reload", Command::Reload, false, cx))
                    .child(self.button(
                        "follow",
                        if follow { "● Following" } else { "Follow" },
                        Command::ToggleFollow,
                        follow,
                        cx,
                    ))
                    .child(self.button("mark", "Bookmark", Command::ToggleBookmark, false, cx))
                    .child(self.button("goto", "Go to line", Command::GoToLine, false, cx))
                    .child(self.button("close", "Close tab", Command::CloseTab, false, cx)),
            )
            .child(tabs)
            .when(self.show_files, |root| {
                let mut library = div()
                    .id("file-library")
                    .flex()
                    .flex_col()
                    .max_h(px(200.))
                    .overflow_y_scroll()
                    .p_3()
                    .gap_1()
                    .bg(rgb(p.panel))
                    .child("FAVORITES & RECENT FILES");
                for (index, path) in self
                    .config
                    .files
                    .favorites
                    .iter()
                    .map(PathBuf::from)
                    .chain(self.recent.iter().cloned())
                    .enumerate()
                {
                    library = library.child(
                        div()
                            .id(("library-file", index))
                            .cursor_pointer()
                            .text_color(rgb(p.accent))
                            .child(path.display().to_string())
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.open(path.clone(), cx)),
                            ),
                    );
                }
                root.child(library)
            })
            .child(
                div()
                    .px_3()
                    .py_1()
                    .overflow_hidden()
                    .text_color(rgb(p.muted))
                    .child(path),
            )
            .when(self.show_goto, |root| {
                root.child(
                    div()
                        .key_context("GoTo")
                        .flex()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .child("Line number:")
                        .child(
                            text_input("goto-input")
                                .key_context(format!("{DEFAULT_INPUT_CONTEXT} GoTo").as_str())
                                .state(self.goto.downgrade())
                                .w(px(160.))
                                .bg(rgb(p.panel)),
                        )
                        .child(self.button("jump", "Jump", Command::GoToLine, true, cx)),
                )
            })
            .child(content)
            .when(self.config.window.show_status_bar, |root| {
                root.child(
                    div()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(rgb(p.border))
                        .text_color(rgb(p.muted))
                        .child(message),
                )
            })
    }
}

pub fn run(config: Config, paths: Vec<PathBuf>) -> anyhow::Result<()> {
    let bindings = keybindings(&config)?;
    gpui_platform::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx: &mut App| {
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

    #[test]
    fn invalid_keybindings_are_errors_instead_of_panics() {
        let mut config = Config::load(None).unwrap();
        config.keyboard.bindings[0].keys = "invalidmodifier-o".into();
        assert!(keybindings(&config).is_err());
    }
}
