//! Inoreader on a Kobo: one sync downloads three streams, merges them into
//! one batch saved on the reader, the shelf lists it under Unread, Starred
//! and History tabs, and a row opens the article in the shared document
//! reader.

mod inoreader;
mod pending;

use inoreader::{Article, Part, Status, MAX_RESPONSE, SNAPSHOT_IDENTITY};
use kobo_bookview::illustrations::Illustrations;
use kobo_bookview::positions::{self, Progress};
use kobo_bookview::{BookView, Step};
use kobo_read::Outcome;
use kobo_sdk::snapshot::{Snapshot, SnapshotEvent};
use kobo_sdk::{
    action_id, ActionId, BannerLevel, Context, Glyph, KoboApp, Position, Screen, ScreenBuilder,
    StoreResult, TaskError, TaskId, TaskOutcome,
};
use pending::{Change, Pending};
use std::process::ExitCode;

const SYNC: &str = "sync";
const RETRY_SAVE: &str = "retry-save";
const PREVIOUS: &str = "previous";
const NEXT: &str = "next";
const UNREAD: &str = "unread";
const STARRED: &str = "starred";
const HISTORY: &str = "history";
const STAR: &str = "star";
const UNSTAR: &str = "unstar";
const MARK_READ: &str = "mark-read";
const KEEP_UNREAD: &str = "keep-unread";

const COULD_NOT_START: &str = "This Kobo could not start the request. Try again.";
const CANCELLED: &str = "Sync cancelled.";
const SYNCING: &str = "Syncing articles…";
const OPENING: &str = "Opening saved articles. Choose Sync when they are ready.";
const UPDATING: &str = "Saved articles are still being updated. Try Sync again shortly.";
const UNREADABLE: &str = "Saved articles could not be read. Sync to download them again.";
const SNAPSHOT_FAILED: &str = "Articles could not be saved or opened. Retry saving before closing.";
const NEW_NOT_SAVED: &str = "New articles are not saved. Retry saving before closing.";
const SENDING: &str = "Sending your changes to Inoreader…";
const QUEUE_FULL: &str = "This Kobo cannot hold more unsent changes. Sync first.";
const NOT_CONFIRMED: &str =
    "Inoreader did not confirm the change. It is kept and goes out on the next sync.";
const QUEUE_NOT_SAVED: &str = "Your changes are not saved on this Kobo yet.";
const POSITIONS_NOT_SAVED: &str = "Reading positions are not saved.";
const IMAGES_NOT_SAVED: &str = "Some article images are not saved.";
const SUMMARY_ONLY: &str =
    "This feed sent a summary only. Open the article on your computer to read the rest.";

/// What the one outstanding task is for.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Awaiting {
    /// The stream at this index of [`Part::ORDER`].
    Sync(usize),
    /// A batch of one kind of change, for these articles.
    Change(Change, Vec<u64>),
}

/// Which slice of the saved batch the shelf lists.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Tab {
    #[default]
    Unread,
    Starred,
    History,
}

impl Tab {
    const ALL: [Self; 3] = [Self::Unread, Self::Starred, Self::History];

    const fn action(self) -> &'static str {
        match self {
            Self::Unread => UNREAD,
            Self::Starred => STARRED,
            Self::History => HISTORY,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Unread => "Unread",
            Self::Starred => "Starred",
            Self::History => "History",
        }
    }

    const fn empty(self) -> (&'static str, &'static str) {
        match self {
            Self::Unread => ("No unread articles", "Sync to check for new ones."),
            Self::Starred => (
                "No starred articles",
                "Star an article in Inoreader and it is kept here.",
            ),
            Self::History => (
                "Nothing read yet",
                "Articles you have read appear here after they are opened.",
            ),
        }
    }

    fn holds(self, status: Status, starred: bool) -> bool {
        match self {
            Self::Unread => status == Status::Unread,
            Self::Starred => starred,
            Self::History => status == Status::Read,
        }
    }
}

struct Inoreader {
    snapshot: Snapshot,
    /// Whether the snapshot has answered its first load.
    loaded: bool,
    articles: Vec<Article>,
    /// The parts of a sync that have arrived so far.
    arriving: Vec<Article>,
    tab: Tab,
    page: usize,
    /// The changes Inoreader has not confirmed.
    pending: Pending,
    /// The article whose row menu is open.
    menu: Option<usize>,
    /// Whether the last send went unanswered.
    unreachable: bool,
    /// Whether a Sync is waiting for its changes to go out first.
    download_after: bool,
    /// What the banner says, if anything.
    notice: Option<(BannerLevel, String)>,
    /// The one outstanding request, if any; answers for any other id are
    /// ignored.
    task: Option<(TaskId, Awaiting)>,
    book: BookView,
    /// Where each article was left.
    progress: Progress,
    /// Pictures the open article names, saved for offline.
    pictures: Illustrations,
    /// The index of the article being read.
    reading: Option<usize>,
}

impl Default for Inoreader {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::new(SNAPSHOT_IDENTITY).at_most(MAX_RESPONSE),
            loaded: false,
            articles: Vec::new(),
            arriving: Vec::new(),
            tab: Tab::default(),
            page: 0,
            pending: Pending::default(),
            menu: None,
            unreachable: false,
            download_after: false,
            notice: None,
            task: None,
            book: BookView::new(),
            progress: Progress::default(),
            pictures: Illustrations::default(),
            reading: None,
        }
    }
}

/// The sentence and banner level for one task failure.
///
/// Attention for the three credential sentences; `Info` for everything
/// else, using the advice `kobo_sdk::Failure` already carries.
fn notice_for(error: TaskError) -> (BannerLevel, String) {
    match error {
        TaskError::NoCredential => (
            BannerLevel::Attention,
            "No credential named inoreader is on this Kobo. On your computer run kobo secret \
             set inoreader --device <address>, then Sync again."
                .to_owned(),
        ),
        TaskError::Unauthorized => (
            BannerLevel::Attention,
            "Inoreader rejected the token. Tokens expire; refresh it on your computer and \
             install it again with kobo secret set inoreader."
                .to_owned(),
        ),
        TaskError::Denied => (
            BannerLevel::Attention,
            "This Kobo's runtime refused the request. Update Cobalt to a release that knows \
             this app."
                .to_owned(),
        ),
        other => (
            BannerLevel::Info,
            kobo_sdk::Failure::of(other).advice.to_owned(),
        ),
    }
}

/// The name the reading positions file keeps an article under.
fn document_id(id: u64) -> String {
    kobo_net::sha256::hex_digest(format!("inoreader:inoreader:{id}").as_bytes())
}

fn article_action(index: usize) -> String {
    format!("article-{index}")
}

fn menu_action(index: usize) -> String {
    format!("article-menu-{index}")
}

fn page_number(page: usize) -> u16 {
    u16::try_from(page.saturating_add(1)).unwrap_or(u16::MAX)
}

impl Inoreader {
    /// The top bar and banner every shelf screen starts with; also what the
    /// pages are measured against.
    fn prefix(&self) -> ScreenBuilder {
        let mut screen = ScreenBuilder::new("inoreader")
            .top_bar("Inoreader")
            .top_bar_glyph(SYNC, "Sync", Glyph::Refresh)
            .tabs(
                Tab::ALL
                    .iter()
                    .position(|tab| *tab == self.tab)
                    .unwrap_or_default(),
                Tab::ALL.map(|tab| (tab.action(), tab.label())),
            );
        if let Some((level, text)) = self.banner() {
            let level = if self.snapshot.retryable() {
                BannerLevel::Attention
            } else {
                level
            };
            screen = screen.banner(level, text);
        }
        screen
    }

    /// What the banner says, in order of what a reader can do something
    /// about: the app's own notice, then a queue that is not kept, then the
    /// changes still waiting for Inoreader.
    fn banner(&self) -> Option<(BannerLevel, String)> {
        if let Some(notice) = &self.notice {
            return Some(notice.clone());
        }
        if self.pending.failed() {
            return Some((BannerLevel::Attention, QUEUE_NOT_SAVED.to_owned()));
        }
        if self.progress.failed {
            return Some((BannerLevel::Attention, POSITIONS_NOT_SAVED.to_owned()));
        }
        // A picture that will not come or cannot be drawn is read as its
        // caption, and saying so on every shelf would be noise; only a save
        // that can be tried again is worth a sentence.
        if self.pictures.can_retry() {
            return Some((BannerLevel::Attention, IMAGES_NOT_SAVED.to_owned()));
        }
        let count = self.pending.len();
        let text = match (count, self.unreachable) {
            (0, _) => return None,
            (1, false) => "1 change waiting for Inoreader.".to_owned(),
            (count, false) => format!("{count} changes waiting for Inoreader."),
            (1, true) => "Inoreader did not answer. 1 change is kept and goes out on the next \
                          sync."
                .to_owned(),
            (count, true) => format!(
                "Inoreader did not answer. {count} changes are kept and go out on the next sync."
            ),
        };
        Some((BannerLevel::Info, text))
    }

    /// The status and star the reader sees: the downloaded article with every
    /// waiting change laid over it.
    fn shown(&self, article: &Article) -> (Status, bool) {
        let mut status = article.status;
        let mut starred = article.starred;
        for change in self.pending.changes_to(article.id) {
            match change {
                Change::Read => status = Status::Read,
                Change::Unread => status = Status::Unread,
                Change::Star => starred = true,
                Change::Unstar => starred = false,
            }
        }
        (status, starred)
    }

    fn shelf_screen(&mut self, context: &Context) -> Screen {
        let mut screen = self.prefix();
        let retry = self.snapshot.retryable()
            || self.pending.failed()
            || self.progress.failed
            || self.pictures.can_retry();
        let tab = self.tab;
        let shown: Vec<usize> = (0..self.articles.len())
            .filter(|&index| {
                let (status, starred) = self.shown(&self.articles[index]);
                tab.holds(status, starred)
            })
            .collect();
        if shown.is_empty() {
            self.menu = None;
            let (title, summary) = tab.empty();
            screen = screen.splash(Some(Glyph::Rss), title, summary);
            if retry {
                screen = screen.bottom_action_marked(RETRY_SAVE, "Retry saving", Glyph::Refresh);
            }
            return screen.build();
        }
        // Rows are measured the way they are drawn: a title clamped to two
        // lines above a summary on one, both leaving room for the menu mark.
        let cells: Vec<(String, String)> = shown
            .iter()
            .map(|&index| {
                let article = &self.articles[index];
                let (status, starred) = self.shown(article);
                let mut summary = article.feed.clone();
                if starred {
                    summary.push_str(" · Starred");
                }
                if tab != Tab::Unread && status == Status::Unread {
                    summary.push_str(" · Unread");
                }
                (
                    context.clamped_row_with_menu(&article.title, 2, retry),
                    context.one_line_row_with_menu(&summary, retry),
                )
            })
            .collect();
        let measured: Vec<(&str, &str)> = cells
            .iter()
            .map(|(title, feed)| (title.as_str(), feed.as_str()))
            .collect();
        let placed = self.prefix().build();
        let mut pages =
            context.paginate_rows_with_menu_under(&measured, retry, Position::Elsewhere, &placed);
        if pages.len() > 1 {
            pages = context.paginate_rows_with_menu_under(
                &measured,
                retry,
                Position::AtTheFoot,
                &placed,
            );
        }
        let last = pages.len().saturating_sub(1);
        self.page = self.page.min(last);
        let indices = pages.get(self.page).cloned().unwrap_or_default();
        screen = screen.rows_with_menu(indices.iter().map(|&at| {
            let index = shown[at];
            let (_, starred) = self.shown(&self.articles[index]);
            (
                article_action(index),
                cells[at].0.clone(),
                cells[at].1.clone(),
                if starred { Glyph::Bookmark } else { Glyph::Rss },
                menu_action(index),
            )
        }));
        // A menu whose row is not on this page has nothing to point at.
        let open = self
            .menu
            .filter(|&index| indices.iter().any(|&at| shown[at] == index));
        if let Some(index) = open {
            let (status, starred) = self.shown(&self.articles[index]);
            let star = if starred {
                (UNSTAR, "Remove star", Glyph::Bookmark)
            } else {
                (STAR, "Star", Glyph::Bookmark)
            };
            let read = if status == Status::Unread {
                (MARK_READ, "Mark read", Glyph::Check)
            } else {
                (KEEP_UNREAD, "Keep unread", Glyph::Circle)
            };
            screen = screen
                .row_overflow(menu_action(index), true, [star, read])
                .owns_back(true);
        }
        if pages.len() > 1 {
            screen = screen
                .page_turns(PREVIOUS, NEXT)
                .page_position(page_number(self.page), page_number(last));
        }
        if retry {
            screen = screen.bottom_action_marked(RETRY_SAVE, "Retry saving", Glyph::Refresh);
        }
        screen.build()
    }

    fn reading_screen(&self, index: usize) -> Screen {
        let title = self
            .articles
            .get(index)
            .map_or("", |article| article.title.as_str());
        // A body with nothing readable in it comes back as a screen with no
        // nodes rather than as `None`, so both mean "summary only".
        self.book
            .screen(title)
            .filter(|screen| !screen.nodes.is_empty())
            .unwrap_or_else(|| {
                ScreenBuilder::new("inoreader-summary")
                    .top_bar(title)
                    .empty_state(SUMMARY_ONLY)
                    .owns_back(true)
                    .build()
            })
    }

    fn show(&mut self, context: &mut Context) {
        let screen = match self.reading {
            Some(index) => self.reading_screen(index),
            None => self.shelf_screen(context),
        };
        context.set_screen(screen);
    }

    fn info(&mut self, text: impl Into<String>) {
        self.notice = Some((BannerLevel::Info, text.into()));
    }

    /// Starts a Sync, unless something is in the way.
    fn sync(&mut self, context: &mut Context) {
        if self.task.is_some() {
            return;
        }
        if !self.snapshot.retryable() {
            if !self.loaded {
                self.info(OPENING);
                self.show(context);
                return;
            }
            if self.snapshot.busy() {
                self.info(UPDATING);
                self.show(context);
                return;
            }
        }
        self.notice = None;
        self.download_after = true;
        if self.pending.next_batch().is_some() {
            self.info(SENDING);
            self.flush(context);
        } else {
            self.download(context);
        }
        self.show(context);
    }

    /// Starts downloading the three streams.
    fn download(&mut self, context: &mut Context) {
        self.download_after = false;
        self.arriving.clear();
        self.fetch(context, 0);
    }

    /// Sends the next batch of changes, if one is waiting and nothing else
    /// is outstanding.
    fn flush(&mut self, context: &mut Context) {
        if self.task.is_some() {
            return;
        }
        let Some((kind, ids)) = self.pending.next_batch() else {
            return;
        };
        if let Some(id) = context.spawn(inoreader::edit_tag(kind, &ids)) {
            self.pending.begin(kind, ids.clone());
            self.task = Some((id, Awaiting::Change(kind, ids)));
        } else {
            self.download_after = false;
            self.info(COULD_NOT_START);
        }
    }

    /// Queues one change to one article and tries to send it.
    fn queue(&mut self, context: &mut Context, index: usize, change: Change) {
        let id = self.articles[index].id;
        self.notice = None;
        if self.pending.push(context, id, change) {
            self.flush(context);
        } else {
            self.info(QUEUE_FULL);
        }
    }

    /// Inoreader confirmed a batch: the articles take the new state and the
    /// changes leave the queue.
    fn confirmed(&mut self, context: &mut Context, kind: Change, ids: &[u64]) {
        for article in self.articles.iter_mut().filter(|a| ids.contains(&a.id)) {
            match kind {
                Change::Read => article.status = Status::Read,
                Change::Unread => article.status = Status::Unread,
                Change::Star => article.starred = true,
                Change::Unstar => article.starred = false,
            }
        }
        self.pending.applied(context, kind, ids);
        self.unreachable = false;
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, text)| text == SENDING || text == NOT_CONFIRMED)
        {
            self.notice = None;
        }
        // A save that cannot start now is not an error: the next Sync
        // downloads the state again.
        self.snapshot
            .save(context, inoreader::encode_saved(&self.articles));
        if self.pending.next_batch().is_some() {
            self.flush(context);
        } else if self.download_after {
            self.download(context);
        }
    }

    /// Asks for the part at `at` in [`Part::ORDER`].
    fn fetch(&mut self, context: &mut Context, at: usize) {
        if let Some(id) = context.spawn_retrying(inoreader::request(Part::ORDER[at])) {
            self.task = Some((id, Awaiting::Sync(at)));
            self.info(SYNCING);
        } else {
            self.arriving.clear();
            self.info(COULD_NOT_START);
        }
    }

    /// Takes one part's reply; the last part replaces the list and saves it.
    /// A part that cannot be read drops what arrived and keeps the saved batch.
    fn synced(&mut self, context: &mut Context, at: usize, bytes: &[u8]) {
        let part = Part::ORDER[at];
        match inoreader::parse_stream(bytes, part) {
            Ok(articles) => inoreader::merge(&mut self.arriving, articles),
            Err(error) => {
                self.arriving.clear();
                self.info(error.sentence());
                return;
            }
        }
        if at + 1 < Part::ORDER.len() {
            self.fetch(context, at + 1);
            return;
        }
        self.articles = std::mem::take(&mut self.arriving);
        self.page = 0;
        let count = self.articles.len();
        self.info(if count == 1 {
            "Synced 1 article.".to_owned()
        } else {
            format!("Synced {count} articles.")
        });
        let saved = self
            .snapshot
            .save(context, inoreader::encode_saved(&self.articles));
        if !saved {
            self.notice = Some((BannerLevel::Attention, NEW_NOT_SAVED.to_owned()));
        }
    }

    /// Takes the answer to a batch of changes.
    fn answered(&mut self, context: &mut Context, kind: Change, ids: &[u64], outcome: TaskOutcome) {
        match outcome {
            TaskOutcome::Completed(bytes) if inoreader::is_ok(&bytes) => {
                self.confirmed(context, kind, ids);
                return;
            }
            TaskOutcome::Completed(_) => {
                self.pending.unresolved();
                self.info(NOT_CONFIRMED);
            }
            TaskOutcome::Failed(error) => {
                self.pending.unresolved();
                self.unreachable = true;
                self.notice = None;
                // The waiting line says the change is kept; only a failure
                // the owner can fix is worth a sentence of its own.
                if matches!(
                    error,
                    TaskError::Unauthorized
                        | TaskError::NoCredential
                        | TaskError::Denied
                        | TaskError::RateLimited(_)
                ) {
                    self.notice = Some(notice_for(error));
                }
            }
            TaskOutcome::Cancelled => {
                self.pending.unresolved();
                self.notice = None;
            }
        }
        self.download_after = false;
    }

    fn snapshot_event(&mut self, event: Option<SnapshotEvent>) {
        match event {
            Some(SnapshotEvent::Loaded) => {
                self.loaded = true;
                if let Some(bytes) = self.snapshot.bytes.as_deref() {
                    match inoreader::parse_saved(bytes) {
                        Ok(articles) => self.articles = articles,
                        Err(_) => self.info(UNREADABLE),
                    }
                }
            }
            Some(SnapshotEvent::Saved) => {
                if self
                    .notice
                    .as_ref()
                    .is_some_and(|(_, text)| text == SNAPSHOT_FAILED || text == NEW_NOT_SAVED)
                {
                    self.notice = None;
                }
            }
            Some(SnapshotEvent::Failed) => {
                self.notice = Some((BannerLevel::Attention, SNAPSHOT_FAILED.to_owned()));
            }
            None => {}
        }
    }

    fn open(&mut self, context: &mut Context, index: usize) {
        let article = &self.articles[index];
        let body = article.content.clone();
        let url = article.url.clone();
        let memory = self.progress.memory(&document_id(article.id));
        self.book
            .open(context, kobo_doc::html::parse(&body), memory);
        if !url.is_empty() {
            self.pictures.open(context, &mut self.book, &url);
        }
        self.reading = Some(index);
        self.menu = None;
        if self.shown(&self.articles[index]).0 == Status::Unread {
            self.queue(context, index, Change::Read);
        }
    }

    /// Remembers where the open article is, if there is one.
    fn keep_place(&mut self, context: &mut Context) {
        if let (Some(index), Some(memory)) = (self.reading, self.book.memory().cloned()) {
            let id = document_id(self.articles[index].id);
            self.progress.keep(context, id, memory);
        }
    }

    fn close(&mut self, context: &mut Context) {
        self.keep_place(context);
        self.pictures.close(context);
        self.book.close(context);
        self.reading = None;
    }

    fn reading_action(&mut self, context: &mut Context, action: ActionId) {
        match self.book.act(context, action) {
            Some(Outcome::Close) => self.close(context),
            None if action == ActionId::BACK => self.close(context),
            Some(Outcome::Light(level)) => {
                self.keep_place(context);
                context.device().set_frontlight(level);
            }
            Some(Outcome::Save) => self.keep_place(context),
            _ => {}
        }
    }
}

impl KoboApp for Inoreader {
    fn on_start(&mut self, context: &mut Context) {
        self.snapshot.start(context);
        Pending::start(context);
        context.store().load(positions::KEY);
        self.show(context);
    }

    fn on_action(&mut self, context: &mut Context, action: ActionId) {
        if self.reading.is_some() {
            self.reading_action(context, action);
        } else if action == action_id(SYNC) {
            self.sync(context);
        } else if action == action_id(RETRY_SAVE) {
            self.snapshot.retry(context);
            if self.pending.failed() {
                self.pending.retry(context);
            }
            if self.progress.failed {
                self.progress.retry(context);
            }
            self.pictures.retry(context);
        } else if action == ActionId::BACK {
            if self.menu.take().is_none() {
                return;
            }
        } else if let Some(tab) = Tab::ALL
            .into_iter()
            .find(|tab| action == action_id(tab.action()))
        {
            self.tab = tab;
            self.page = 0;
            self.menu = None;
        } else if let Some(change) = self.menu.and_then(|index| {
            [
                (STAR, Change::Star),
                (UNSTAR, Change::Unstar),
                (MARK_READ, Change::Read),
                (KEEP_UNREAD, Change::Unread),
            ]
            .into_iter()
            .find(|(name, _)| action == action_id(name))
            .map(|(_, change)| (index, change))
        }) {
            self.menu = None;
            self.queue(context, change.0, change.1);
        } else if let Some(index) =
            (0..self.articles.len()).find(|&index| action == action_id(&menu_action(index)))
        {
            self.menu = if self.menu == Some(index) {
                None
            } else {
                Some(index)
            };
        } else if action == action_id(PREVIOUS) {
            self.page = self.page.saturating_sub(1);
        } else if action == action_id(NEXT) {
            self.page = self.page.saturating_add(1);
        } else if let Some(index) =
            (0..self.articles.len()).find(|&index| action == action_id(&article_action(index)))
        {
            self.open(context, index);
        } else {
            return;
        }
        self.show(context);
    }

    fn on_task(&mut self, context: &mut Context, task: TaskId, outcome: TaskOutcome) {
        if self.pictures.task(context, &mut self.book, task, &outcome) {
            self.show(context);
            return;
        }
        if self.book.woke(context, task, &outcome) != Step::Elsewhere {
            if self.reading.is_some() {
                self.show(context);
            }
            return;
        }
        let Some((outstanding, awaiting)) = self.task.clone() else {
            return;
        };
        if outstanding != task {
            return;
        }
        self.task = None;
        match awaiting {
            Awaiting::Sync(at) => match outcome {
                TaskOutcome::Completed(bytes) => self.synced(context, at, &bytes),
                TaskOutcome::Failed(error) => {
                    self.arriving.clear();
                    self.notice = Some(notice_for(error));
                }
                TaskOutcome::Cancelled => {
                    self.arriving.clear();
                    self.info(CANCELLED);
                }
            },
            Awaiting::Change(kind, ids) => self.answered(context, kind, &ids, outcome),
        }
        self.show(context);
    }

    fn on_load(&mut self, context: &mut Context, key: &str, result: StoreResult) {
        if self
            .pictures
            .store(context, &mut self.book, key, &result, false)
        {
            self.show(context);
        } else if key == positions::KEY {
            self.progress.load(result);
            self.show(context);
        } else if key == self.snapshot.key {
            let event = self.snapshot.stored(context, &result);
            self.snapshot_event(event);
            self.show(context);
        } else if key == pending::KEY {
            let was_loaded = self.pending.loaded();
            self.pending.stored(context, &result);
            if !was_loaded && self.pending.loaded() {
                self.flush(context);
            }
            self.show(context);
        }
    }

    fn on_save(&mut self, context: &mut Context, key: &str, result: StoreResult) {
        if key == positions::KEY {
            self.progress.saved(context, result);
            self.show(context);
        } else {
            self.on_load(context, key, result);
        }
    }

    fn on_shelf(&mut self, context: &mut Context, name: &str, result: StoreResult) {
        if self
            .pictures
            .store(context, &mut self.book, name, &result, true)
        {
            self.show(context);
        } else if self.snapshot.owns_file(name) {
            let event = self.snapshot.shelf(context, &result);
            self.snapshot_event(event);
            self.show(context);
        }
    }
}

fn main() -> ExitCode {
    match kobo_sdk::run("inoreader-client", Inoreader::default()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("inoreader-client: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        article_action, inoreader, menu_action, pending, positions, Inoreader, HISTORY,
        KEEP_UNREAD, MARK_READ, NEXT, PREVIOUS, RETRY_SAVE, STAR, STARRED, SYNC, UNREAD, UNSTAR,
    };
    use kobo_sdk::{
        action_id, ActionId, AppRunner, Command, Context, DisplayMetrics, KoboApp, StoreError,
        StoreRequest, StoreResult, Task, TaskError, TaskOutcome,
    };
    use kobo_ui::{DiagnosticSeverity, Node, Screen, TextScale};

    /// One real-shaped item: a quote in the title, a canonical link, and
    /// categories carrying a numeric user id.
    const STREAM: &str = r#"{"id":"x","items":[{
        "id":"tag:google.com,2005:reader/item/0000000bcaa77f5b",
        "title":"A headline",
        "summary":{"direction":"ltr","content":"<p>Body of the article.</p>"},
        "canonical":[{"href":"https://example.com/a"}],
        "origin":{"title":"Example Feed"},
        "categories":["user/1006150148/state/com.google/reading-list"]
    }]}"#;

    /// The Starred stream: one item the Unread stream also holds, one only
    /// starred (and already read).
    const STARRED_STREAM: &str = r#"{"id":"x","items":[
        {"id":"tag:google.com,2005:reader/item/0000000bcaa77f5b","title":"A headline",
         "origin":{"title":"Example Feed"}},
        {"id":"tag:google.com,2005:reader/item/0000000000000002","title":"A kept one",
         "summary":{"content":"<p>Kept.</p>"},"origin":{"title":"Other Feed"}}]}"#;

    /// The Read stream: the starred one again, and one more.
    const READ_STREAM: &str = r#"{"id":"x","items":[
        {"id":"tag:google.com,2005:reader/item/0000000000000002","title":"A kept one"},
        {"id":"tag:google.com,2005:reader/item/0000000000000003","title":"An old one",
         "origin":{"title":"Third Feed"}}]}"#;

    fn three() -> Vec<TaskOutcome> {
        [STREAM, STARRED_STREAM, READ_STREAM]
            .map(|body| TaskOutcome::Completed(body.as_bytes().to_vec()))
            .to_vec()
    }

    /// Every string the screen carries, joined so a test can look for a
    /// substring without caring which node held it.
    fn screen_text(screen: &Screen) -> String {
        let mut out = String::new();
        if let Some(bar) = &screen.top_bar {
            out.push_str(&bar.title);
            out.push('\n');
        }
        for node in &screen.nodes {
            match node {
                Node::Heading { text, .. }
                | Node::Text { text, .. }
                | Node::Secondary { text, .. }
                | Node::Banner { text, .. } => out.push_str(text),
                Node::Splash { title, summary, .. } => {
                    out.push_str(title);
                    out.push(' ');
                    out.push_str(summary);
                }
                Node::Rows { rows, .. } => {
                    for row in rows {
                        out.push_str(&row.title);
                        out.push(' ');
                        out.push_str(&row.summary);
                        out.push('\n');
                    }
                }
                _ => {}
            }
            out.push('\n');
        }
        out
    }

    fn last_screen(commands: &[Command]) -> Option<Screen> {
        commands.iter().rev().find_map(|command| match command {
            Command::SetScreen(screen) => Some(screen.clone()),
            _ => None,
        })
    }

    fn spawned(commands: &[Command]) -> Vec<(kobo_sdk::TaskId, Task)> {
        commands
            .iter()
            .filter_map(|command| match command {
                Command::Spawn { task, work } => Some((*task, work.clone())),
                _ => None,
            })
            .collect()
    }

    /// A started runner whose snapshot has answered "nothing saved yet".
    fn runner() -> AppRunner<Inoreader> {
        let mut runner = AppRunner::new(Inoreader::default());
        runner.start();
        let key = runner.app().snapshot.key.clone();
        runner.store_result(StoreResult::Loaded { key, value: None });
        runner.store_result(StoreResult::Loaded {
            key: pending::KEY.to_owned(),
            value: None,
        });
        runner.store_result(StoreResult::Loaded {
            key: positions::KEY.to_owned(),
            value: None,
        });
        runner
    }

    /// Taps Sync and answers each request in turn, returning the commands
    /// the last answer produced. Each answer must have asked for exactly one
    /// more request, except the last.
    fn sync_with(runner: &mut AppRunner<Inoreader>, outcomes: Vec<TaskOutcome>) -> Vec<Command> {
        let mut commands = runner.action(action_id(SYNC));
        let total = outcomes.len();
        for (at, outcome) in outcomes.into_iter().enumerate() {
            let fetches = spawned(&commands);
            assert_eq!(fetches.len(), 1, "part {at} did not make one request");
            let Task::Fetch { url, .. } = &fetches[0].1 else {
                panic!("not a fetch")
            };
            if total == 3 {
                assert_eq!(url, inoreader::Part::ORDER[at].url());
            }
            commands = runner.task_outcome(fetches[0].0, outcome);
        }
        commands
    }

    fn shelf_write(commands: &[Command]) -> Option<Vec<u8>> {
        commands.iter().find_map(|command| match command {
            Command::Store(StoreRequest::ShelfWrite { bytes, .. }) => Some(bytes.clone()),
            _ => None,
        })
    }

    /// Answers the write the snapshot asked for, then the pointer save.
    fn acknowledge(runner: &mut AppRunner<Inoreader>, written: &[u8]) {
        let file = shelf_name(runner);
        runner.store_result(StoreResult::ShelfWritten {
            name: file,
            size: u32::try_from(written.len()).expect("small"),
        });
        let key = runner.app().snapshot.key.clone();
        runner.store_result(StoreResult::Saved { key });
    }

    fn shelf_name(runner: &AppRunner<Inoreader>) -> String {
        let stem = kobo_net::sha256::hex_digest(inoreader::SNAPSHOT_IDENTITY.as_bytes());
        [0, 1]
            .iter()
            .map(|slot| format!("{}.{slot}", &stem[..60]))
            .find(|name| runner.app().snapshot.owns_file(name))
            .expect("the snapshot owns a file")
    }

    fn synced_runner() -> AppRunner<Inoreader> {
        let mut runner = runner();
        let commands = sync_with(&mut runner, three());
        let bytes = shelf_write(&commands).expect("the batch was written");
        acknowledge(&mut runner, &bytes);
        runner
    }

    #[test]
    fn each_credential_failure_gets_its_own_sentence() {
        // A raw app and Context, not `AppRunner`: `Offline` is worth a silent
        // retry, and that orchestration is not what this test is about.
        let cases = [
            (TaskError::NoCredential, "kobo secret set inoreader"),
            (TaskError::Unauthorized, "rejected the token"),
            (TaskError::Denied, "runtime refused"),
            (TaskError::Offline, "Wi-Fi"),
        ];
        for (error, expected) in cases {
            let mut app = Inoreader::default();
            let mut context = Context::default();
            app.on_action(&mut context, action_id(SYNC));
            let key = app.snapshot.key.clone();
            app.on_load(
                &mut context,
                &key,
                StoreResult::Loaded {
                    key: key.clone(),
                    value: None,
                },
            );
            app.on_action(&mut context, action_id(SYNC));
            let fetches = spawned(&context.take_commands());
            let (task, _) = fetches.first().expect("Sync did not spawn a fetch");
            app.on_task(&mut context, *task, TaskOutcome::Failed(error));
            let screen = last_screen(&context.take_commands()).expect("a screen");
            let text = screen_text(&screen);
            assert!(text.contains(expected), "{error:?}: {text}");
            assert!(app.articles.is_empty());
        }
    }

    #[test]
    fn a_sync_collects_the_three_streams_the_tabs_are_drawn_from() {
        let mut runner = runner();
        let commands = sync_with(&mut runner, three());
        let bytes = shelf_write(&commands).expect("the batch was written to the shelf");
        let saved = inoreader::parse_saved(&bytes).expect("what was written reads back");
        let ids: Vec<u64> = saved.iter().map(|article| article.id).collect();
        assert_eq!(ids, vec![50_644_615_003, 2, 3]);
        assert!(runner.app().task.is_none());
        let text = screen_text(&last_screen(&commands).expect("a screen"));
        assert!(text.contains("Synced 3 articles."), "{text}");
        assert!(text.contains("A headline"), "{text}");
        assert!(!text.contains("An old one"), "{text}");
        assert!(!text.contains("A kept one"), "{text}");

        let starred =
            screen_text(&last_screen(&runner.action(action_id(STARRED))).expect("a screen"));
        assert!(starred.contains("A headline"), "{starred}");
        assert!(starred.contains("A kept one"), "{starred}");
        assert!(
            starred.contains("Example Feed · Starred · Unread"),
            "{starred}"
        );
        assert!(starred.contains("Other Feed · Starred"), "{starred}");
        assert!(!starred.contains("An old one"), "{starred}");

        let history =
            screen_text(&last_screen(&runner.action(action_id(HISTORY))).expect("a screen"));
        assert!(history.contains("A kept one"), "{history}");
        assert!(history.contains("An old one"), "{history}");
        assert!(!history.contains("A headline"), "{history}");

        let unread =
            screen_text(&last_screen(&runner.action(action_id(UNREAD))).expect("a screen"));
        assert!(unread.contains("A headline"), "{unread}");
    }

    #[test]
    fn an_item_in_two_streams_is_held_once_and_unread_wins() {
        let mut runner = runner();
        sync_with(&mut runner, three());
        let held: Vec<_> = runner
            .app()
            .articles
            .iter()
            .filter(|article| article.id == 50_644_615_003)
            .collect();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].status, inoreader::Status::Unread);
        assert!(held[0].starred);
    }

    #[test]
    fn a_part_that_fails_leaves_the_saved_batch_alone() {
        let before = synced_runner().app().articles.clone();
        // A raw app and Context, not `AppRunner`: an `Offline` failure is
        // retried silently there.
        let mut app = Inoreader::default();
        let mut context = Context::default();
        let key = app.snapshot.key.clone();
        app.on_load(
            &mut context,
            &key.clone(),
            StoreResult::Loaded { key, value: None },
        );
        app.articles = before.clone();
        app.on_action(&mut context, action_id(SYNC));
        let fetches = spawned(&context.take_commands());
        app.on_task(
            &mut context,
            fetches[0].0,
            TaskOutcome::Completed(STREAM.as_bytes().to_vec()),
        );
        let fetches = spawned(&context.take_commands());
        assert_eq!(fetches.len(), 1, "the second part was not requested");
        app.on_task(
            &mut context,
            fetches[0].0,
            TaskOutcome::Failed(TaskError::Offline),
        );
        let commands = context.take_commands();
        assert!(shelf_write(&commands).is_none());
        assert_eq!(app.articles, before);
        assert!(app.arriving.is_empty());
        assert!(app.task.is_none());
        let text = screen_text(&last_screen(&commands).expect("a screen"));
        assert!(text.contains("Wi-Fi"), "{text}");
    }

    #[test]
    fn the_saved_batch_reopens_after_a_restart_without_a_request() {
        let mut runner = synced_runner();
        let saved = runner.app().snapshot.bytes.clone().expect("published");
        let articles = runner.app().articles.clone();
        let pointer = format!("1:{}", kobo_net::sha256::hex_digest(&saved)).into_bytes();

        let mut fresh = AppRunner::new(Inoreader::default());
        let started = fresh.start();
        assert!(spawned(&started).is_empty());
        let key = fresh.app().snapshot.key.clone();
        fresh.store_result(StoreResult::Loaded {
            key,
            value: Some(pointer),
        });
        fresh.store_result(StoreResult::Loaded {
            key: pending::KEY.to_owned(),
            value: None,
        });
        fresh.store_result(StoreResult::Loaded {
            key: positions::KEY.to_owned(),
            value: None,
        });
        let name = shelf_name(&fresh);
        let commands = fresh.store_result(StoreResult::ShelfRead {
            name,
            offset: 0,
            size: u32::try_from(saved.len()).expect("small"),
            bytes: saved,
        });
        assert!(spawned(&commands).is_empty(), "the restart made a request");
        assert_eq!(fresh.app().articles, articles);
        assert!(fresh.app().articles[0]
            .content
            .contains("Body of the article"));
        let _ = &mut runner;
    }

    #[test]
    fn a_failed_save_keeps_the_articles_and_retry_does_not_download_again() {
        let mut runner = runner();
        let commands = sync_with(&mut runner, three());
        assert!(shelf_write(&commands).is_some());
        let commands = runner.store_result(StoreResult::Denied(StoreError::NoRoom));
        assert!(runner.app().snapshot.retryable());
        assert_eq!(runner.app().articles.len(), 3);
        let screen = last_screen(&commands).expect("a screen");
        assert!(screen_text(&screen).contains("could not be saved"));
        assert!(screen.bottom_action.is_some(), "no Retry saving action");

        let commands = runner.action(action_id(RETRY_SAVE));
        assert!(spawned(&commands).is_empty());
        let key = runner.app().snapshot.key.clone();
        assert!(commands.iter().any(|command| matches!(
            command,
            Command::Store(StoreRequest::Load { key: asked }) if *asked == key
        )));
    }

    #[test]
    fn a_malformed_answer_preserves_the_previous_articles_and_reports_why() {
        let mut runner = synced_runner();
        let before = runner.app().articles.clone();
        let commands = sync_with(
            &mut runner,
            vec![TaskOutcome::Completed(b"<html>Log in</html>".to_vec())],
        );
        assert_eq!(runner.app().articles, before);
        let text = screen_text(&last_screen(&commands).expect("a screen"));
        assert!(text.contains("valid response"), "{text}");
        assert!(text.contains("A headline"), "{text}");
    }

    #[test]
    fn a_failed_sync_keeps_the_previous_articles() {
        let before = synced_runner().app().articles.clone();
        // A raw app and Context, not `AppRunner`: an `Offline` failure is
        // retried silently there.
        let mut app = Inoreader::default();
        let mut context = Context::default();
        let key = app.snapshot.key.clone();
        app.on_load(
            &mut context,
            &key.clone(),
            StoreResult::Loaded { key, value: None },
        );
        app.articles = before.clone();
        app.on_action(&mut context, action_id(SYNC));
        let fetches = spawned(&context.take_commands());
        app.on_task(
            &mut context,
            fetches[0].0,
            TaskOutcome::Failed(TaskError::Offline),
        );
        assert_eq!(app.articles, before);
        let text = screen_text(&last_screen(&context.take_commands()).expect("a screen"));
        assert!(text.contains("Wi-Fi"), "{text}");
    }

    #[test]
    fn opening_an_article_shows_the_reader_and_back_returns_to_the_list() {
        let mut runner = synced_runner();
        let commands = runner.action(action_id(&article_action(0)));
        let screen = last_screen(&commands).expect("a screen");
        assert_eq!(
            screen.top_bar.as_ref().map(|bar| bar.title.as_str()),
            Some("A headline")
        );
        assert!(runner.app().reading.is_some());
        let commands = runner.action(ActionId::BACK);
        let screen = last_screen(&commands).expect("a screen");
        assert!(runner.app().reading.is_none());
        assert_eq!(
            screen.top_bar.as_ref().map(|bar| bar.title.as_str()),
            Some("Inoreader")
        );
    }

    #[test]
    fn a_summary_only_article_says_so() {
        let mut app = Inoreader {
            articles: inoreader::parse_stream(
                br#"{"items":[{"id":"tag:google.com,2005:reader/item/0000000000000009","title":"T"}]}"#,
                inoreader::Part::Unread,
            )
            .expect("parses"),
            loaded: true,
            ..Inoreader::default()
        };
        let mut context = Context::default();
        app.on_action(&mut context, action_id(&article_action(0)));
        let screen = last_screen(&context.take_commands()).expect("a screen");
        assert!(screen_text(&screen).contains("summary only"), "{screen:#?}");
        assert!(screen.owns_back);
    }

    const READ_TAG: &str = "user/-/state/com.google/read";
    const STARRED_TAG: &str = "user/-/state/com.google/starred";

    fn item(number: u32) -> String {
        format!(
            r#"{{"id":"tag:google.com,2005:reader/item/{number:016x}","title":"Headline {number}",
            "summary":{{"content":"<p>Body {number}.</p>"}},"origin":{{"title":"Feed"}}}}"#
        )
    }

    fn stream(numbers: &[u32]) -> TaskOutcome {
        let items: Vec<String> = numbers.iter().map(|number| item(*number)).collect();
        TaskOutcome::Completed(format!(r#"{{"items":[{}]}}"#, items.join(",")).into_bytes())
    }

    /// A runner holding these unread articles and nothing else.
    fn holding(unread: &[u32], starred: &[u32]) -> AppRunner<Inoreader> {
        let mut runner = runner();
        let commands = sync_with(
            &mut runner,
            vec![stream(unread), stream(starred), stream(&[])],
        );
        let bytes = shelf_write(&commands).expect("the batch was written");
        acknowledge(&mut runner, &bytes);
        runner
    }

    fn posts(commands: &[Command]) -> Vec<(kobo_sdk::TaskId, String)> {
        spawned(commands)
            .into_iter()
            .filter_map(|(id, task)| match task {
                Task::Post { body, .. } => Some((id, body)),
                _ => None,
            })
            .collect()
    }

    fn ok() -> TaskOutcome {
        TaskOutcome::Completed(b"OK".to_vec())
    }

    fn queue_write(commands: &[Command]) -> Option<String> {
        commands.iter().find_map(|command| match command {
            Command::Store(StoreRequest::Save { key, value }) if key == pending::KEY => {
                Some(String::from_utf8(value.clone()).expect("text"))
            }
            _ => None,
        })
    }

    fn shelf_text(runner: &mut AppRunner<Inoreader>, tab: &str) -> String {
        runner.action(action_id(tab));
        let context = runner.context();
        screen_text(&runner.app_mut().shelf_screen(&context))
    }

    #[test]
    fn opening_an_article_marks_it_read_once_and_moves_it_out_of_unread() {
        let mut runner = synced_runner();
        let commands = runner.action(action_id(&article_action(0)));
        let sent = posts(&commands);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, format!("a={READ_TAG}&i=50644615003"));
        assert!(queue_write(&commands).is_some(), "the change was not kept");
        let commands = runner.task_outcome(sent[0].0, ok());
        assert!(posts(&commands).is_empty());
        assert_eq!(runner.app().pending.len(), 0);
        runner.action(ActionId::BACK);
        let unread = shelf_text(&mut runner, UNREAD);
        assert!(!unread.contains("A headline"), "{unread}");
        let history = shelf_text(&mut runner, HISTORY);
        assert!(history.contains("A headline"), "{history}");
        // Opening it again asks for nothing.
        let commands = runner.action(action_id(&article_action(0)));
        assert!(posts(&commands).is_empty());
    }

    #[test]
    fn reading_three_articles_offline_sends_one_post_when_wifi_returns() {
        let mut runner = holding(&[1, 2, 3], &[]);
        let first = posts(&runner.action(action_id(&article_action(0))));
        assert_eq!(first.len(), 1);
        for index in [1, 2] {
            runner.action(ActionId::BACK);
            let commands = runner.action(action_id(&article_action(index)));
            assert!(posts(&commands).is_empty(), "a second request went out");
        }
        runner.action(ActionId::BACK);
        runner.task_outcome(first[0].0, TaskOutcome::Failed(TaskError::Unreachable));
        assert_eq!(runner.app().pending.len(), 3);

        let commands = runner.action(action_id(SYNC));
        assert!(spawned(&commands)
            .iter()
            .all(|(_, task)| matches!(task, Task::Post { .. })));
        let sent = posts(&commands);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1.matches("&i=").count(), 3, "{}", sent[0].1);
        let commands = runner.task_outcome(sent[0].0, ok());
        let fetches = spawned(&commands);
        assert_eq!(fetches.len(), 1);
        let Task::Fetch { url, .. } = &fetches[0].1 else {
            panic!("not a fetch")
        };
        assert_eq!(url, inoreader::Part::Unread.url());
        assert_eq!(runner.app().pending.len(), 0);
    }

    #[test]
    fn an_unstar_made_offline_is_shown_kept_and_survives_a_restart() {
        let mut runner = holding(&[1], &[1]);
        let id = runner.app().articles[0].id;
        assert!(runner.app().articles[0].starred);
        runner.action(action_id(&menu_action(0)));
        let commands = runner.action(action_id(UNSTAR));
        let queued = queue_write(&commands).expect("the queue was saved");
        assert!(queued.contains(&format!("{id}\tunstar")), "{queued}");
        let sent = posts(&commands);
        assert_eq!(sent[0].1, format!("r={STARRED_TAG}&i={id}"));
        // Shown off at once; the stored flag waits for Inoreader.
        assert!(runner.app().articles[0].starred);
        let starred = shelf_text(&mut runner, STARRED);
        assert!(!starred.contains("Headline 1"), "{starred}");

        let mut fresh = AppRunner::new(Inoreader::default());
        fresh.start();
        let saved = runner.app().snapshot.bytes.clone().expect("published");
        let key = fresh.app().snapshot.key.clone();
        let pointer = format!("1:{}", kobo_net::sha256::hex_digest(&saved)).into_bytes();
        fresh.store_result(StoreResult::Loaded {
            key,
            value: Some(pointer),
        });
        fresh.store_result(StoreResult::Loaded {
            key: pending::KEY.to_owned(),
            value: Some(queued.into_bytes()),
        });
        fresh.store_result(StoreResult::Loaded {
            key: positions::KEY.to_owned(),
            value: None,
        });
        let name = shelf_name(&fresh);
        fresh.store_result(StoreResult::ShelfRead {
            name,
            offset: 0,
            size: u32::try_from(saved.len()).expect("small"),
            bytes: saved,
        });
        assert_eq!(fresh.app().pending.len(), 1);
        assert!(fresh.app().articles[0].starred);
        let starred = shelf_text(&mut fresh, STARRED);
        assert!(!starred.contains("Headline 1"), "{starred}");
        assert!(starred.contains("waiting for Inoreader"), "{starred}");
    }

    #[test]
    fn a_change_whose_reply_never_arrived_goes_out_again_on_the_next_sync() {
        let mut runner = holding(&[1], &[]);
        let sent = posts(&runner.action(action_id(&article_action(0))));
        runner.task_outcome(sent[0].0, TaskOutcome::Failed(TaskError::Unreachable));
        let commands = runner.action(ActionId::BACK);
        let text = screen_text(&last_screen(&commands).expect("a screen"));
        assert!(text.contains("goes out on the next sync"), "{text}");
        assert_eq!(runner.app().pending.len(), 1);
        let commands = runner.action(action_id(SYNC));
        let again = posts(&commands);
        assert_eq!(again.len(), 1, "the change was not sent before the fetch");
        assert_eq!(again[0].1, sent[0].1);
        let commands = runner.task_outcome(again[0].0, ok());
        assert_eq!(spawned(&commands).len(), 1);
        assert_eq!(runner.app().pending.len(), 0);
    }

    #[test]
    fn a_reply_that_is_not_ok_keeps_the_change_queued() {
        let mut runner = holding(&[1], &[]);
        let sent = posts(&runner.action(action_id(&article_action(0))));
        runner.task_outcome(sent[0].0, TaskOutcome::Completed(b"{}".to_vec()));
        let commands = runner.action(ActionId::BACK);
        let text = screen_text(&last_screen(&commands).expect("a screen"));
        assert!(text.contains("did not confirm"), "{text}");
        assert_eq!(runner.app().pending.len(), 1);
        assert_eq!(runner.app().articles[0].status, inoreader::Status::Unread);
    }

    #[test]
    fn a_rejected_token_names_itself_and_keeps_the_change() {
        let mut runner = holding(&[1], &[]);
        let sent = posts(&runner.action(action_id(&article_action(0))));
        runner.task_outcome(sent[0].0, TaskOutcome::Failed(TaskError::Unauthorized));
        let commands = runner.action(ActionId::BACK);
        let text = screen_text(&last_screen(&commands).expect("a screen"));
        assert!(text.contains("rejected the token"), "{text}");
        assert_eq!(runner.app().pending.len(), 1);
        // A failed change ends the Sync: nothing is downloaded over it.
        let commands = runner.action(action_id(SYNC));
        let again = posts(&commands);
        let commands =
            runner.task_outcome(again[0].0, TaskOutcome::Failed(TaskError::Unauthorized));
        assert!(
            spawned(&commands).is_empty(),
            "downloaded over a waiting change"
        );
    }

    #[test]
    fn the_row_menu_offers_what_the_article_is_and_back_only_closes_it() {
        let mut runner = holding(&[1, 2], &[2]);
        let commands = runner.action(action_id(&menu_action(0)));
        let screen = last_screen(&commands).expect("a screen");
        assert!(screen.owns_back);
        let diagnostics =
            screen.diagnostics(&kobo_ui::CLARA_BW_METRICS, &kobo_sdk::Chrome::default());
        for name in [STAR, MARK_READ] {
            assert!(
                diagnostics.layout.rect_of_action(action_id(name)).is_some(),
                "{name}"
            );
        }
        assert!(diagnostics
            .layout
            .rect_of_action(action_id(UNSTAR))
            .is_none());
        // Back closes the menu and stays on the shelf.
        let commands = runner.action(ActionId::BACK);
        let screen = last_screen(&commands).expect("a screen");
        assert!(!screen.owns_back);
        assert!(runner.app().menu.is_none());
        // The starred article offers to remove the star.
        runner.action(action_id(&menu_action(1)));
        let commands = runner.action(action_id(&menu_action(1)));
        assert!(last_screen(&commands).is_some_and(|screen| !screen.owns_back));
        let commands = runner.action(action_id(&menu_action(1)));
        let screen = last_screen(&commands).expect("a screen");
        let diagnostics =
            screen.diagnostics(&kobo_ui::CLARA_BW_METRICS, &kobo_sdk::Chrome::default());
        assert!(diagnostics
            .layout
            .rect_of_action(action_id(UNSTAR))
            .is_some());
        // Switching tab closes it.
        runner.action(action_id(HISTORY));
        assert!(runner.app().menu.is_none());
        // Keep unread on a read article, from History.
        runner.action(action_id(UNREAD));
        runner.action(action_id(&menu_action(0)));
        let commands = runner.action(action_id(MARK_READ));
        let sent = posts(&commands);
        assert_eq!(sent[0].1, format!("a={READ_TAG}&i=1"));
        runner.task_outcome(sent[0].0, ok());
        runner.action(action_id(HISTORY));
        let commands = runner.action(action_id(&menu_action(0)));
        let screen = last_screen(&commands).expect("a screen");
        let diagnostics =
            screen.diagnostics(&kobo_ui::CLARA_BW_METRICS, &kobo_sdk::Chrome::default());
        assert!(diagnostics
            .layout
            .rect_of_action(action_id(KEEP_UNREAD))
            .is_some());
    }

    #[test]
    fn the_row_menu_fits_at_every_text_size() {
        let mut failures = Vec::new();
        for scale in TextScale::STEPS {
            let metrics = DisplayMetrics {
                text_scale: scale,
                ..kobo_ui::CLARA_BW_METRICS
            };
            let mut runner = AppRunner::with_metrics(Inoreader::default(), metrics);
            runner.start();
            let key = runner.app().snapshot.key.clone();
            runner.store_result(StoreResult::Loaded { key, value: None });
            runner.store_result(StoreResult::Loaded {
                key: pending::KEY.to_owned(),
                value: None,
            });
            runner.store_result(StoreResult::Loaded {
                key: positions::KEY.to_owned(),
                value: None,
            });
            sync_with(
                &mut runner,
                vec![stream(&[1, 2, 3, 4, 5, 6]), stream(&[2]), stream(&[])],
            );
            runner.app_mut().unreachable = true;
            let commands = runner.action(action_id(&menu_action(1)));
            let screen = last_screen(&commands).expect("a screen");
            let diagnostics = screen.diagnostics(&metrics, &kobo_sdk::Chrome::measuring(false));
            if diagnostics
                .issues
                .iter()
                .any(|issue| issue.severity == DiagnosticSeverity::Error)
            {
                failures.push(format!("{scale:?}: layout error"));
            }
            for name in [UNSTAR, MARK_READ] {
                if diagnostics.layout.rect_of_action(action_id(name)).is_none() {
                    failures.push(format!("{scale:?}: no {name}"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn every_article_is_reachable_through_measured_pages_at_all_text_sizes() {
        let title = "A long headline that runs on well past the end of the first line and into \
                     the second so that every row is two lines tall";
        let articles: Vec<inoreader::Article> = (1..=60)
            .map(|number| inoreader::Article {
                id: number,
                title: format!("{title} {number}"),
                feed: "A feed with a fairly long name".to_owned(),
                content: String::new(),
                url: String::new(),
                starred: number % 7 == 0,
                status: inoreader::Status::Unread,
            })
            .collect();
        let mut failures = Vec::new();
        for scale in TextScale::STEPS {
            let metrics = DisplayMetrics {
                text_scale: scale,
                ..kobo_ui::CLARA_BW_METRICS
            };
            let app = Inoreader {
                articles: articles.clone(),
                loaded: true,
                notice: Some((
                    kobo_sdk::BannerLevel::Attention,
                    super::notice_for(TaskError::NoCredential).1,
                )),
                ..Inoreader::default()
            };
            let mut runner = AppRunner::with_metrics(app, metrics);
            let mut screens = vec![last_screen(&runner.start()).expect("a screen")];
            let mut turn = runner.action(action_id(PREVIOUS));
            screens.extend(last_screen(&turn));
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..80 {
                let previous = screens.last().cloned();
                turn = runner.action(action_id(NEXT));
                let Some(screen) = last_screen(&turn) else {
                    break;
                };
                let same = previous.as_ref() == Some(&screen);
                screens.push(screen);
                if same {
                    break;
                }
            }
            for screen in &screens {
                let diagnostics = screen.diagnostics(&metrics, &kobo_sdk::Chrome::measuring(false));
                if diagnostics
                    .issues
                    .iter()
                    .any(|issue| issue.severity == DiagnosticSeverity::Error)
                {
                    failures.push(format!("{scale:?}: layout error"));
                }
                for id in [SYNC, UNREAD, STARRED, HISTORY] {
                    if diagnostics.layout.rect_of_action(action_id(id)).is_none() {
                        failures.push(format!("{scale:?}: no {id} target"));
                    }
                }
                for index in 0..60 {
                    if diagnostics
                        .layout
                        .rect_of_action(action_id(&article_action(index)))
                        .is_some()
                    {
                        seen.insert(index);
                    }
                }
            }
            if seen.len() != 60 {
                failures.push(format!("{scale:?}: only {} of 60 reachable", seen.len()));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    const STORY: &str = "https://example.com/story";

    /// One article: `paragraphs` of text, then one picture the site serves
    /// from a root-relative path.
    fn story(url: &str, paragraphs: usize) -> TaskOutcome {
        let body = "<p>Text</p>".repeat(paragraphs) + r#"<img src=\"/images/a.png\" alt=\"A\">"#;
        let canonical = if url.is_empty() {
            String::new()
        } else {
            format!(r#""canonical":[{{"href":"{url}"}}],"#)
        };
        TaskOutcome::Completed(
            format!(
                r#"{{"items":[{{"id":"tag:google.com,2005:reader/item/0000000000000007",
                "title":"Story",{canonical}"summary":{{"content":"{body}"}},
                "origin":{{"title":"Feed"}}}}]}}"#
            )
            .into_bytes(),
        )
    }

    fn holding_story(url: &str, paragraphs: usize) -> AppRunner<Inoreader> {
        let mut runner = runner();
        let commands = sync_with(
            &mut runner,
            vec![story(url, paragraphs), stream(&[]), stream(&[])],
        );
        let bytes = shelf_write(&commands).expect("the batch was written");
        acknowledge(&mut runner, &bytes);
        runner
    }

    fn fetches_of(
        commands: &[Command],
    ) -> Vec<(kobo_sdk::TaskId, String, Option<kobo_sdk::Credential>)> {
        spawned(commands)
            .into_iter()
            .filter_map(|(id, task)| match task {
                Task::Fetch {
                    url, credential, ..
                } => Some((id, url, credential)),
                _ => None,
            })
            .collect()
    }

    /// A restarted app that has this batch and answers the position load.
    fn restarted_with(
        runner: &AppRunner<Inoreader>,
        positions: Option<Vec<u8>>,
    ) -> AppRunner<Inoreader> {
        let mut fresh = AppRunner::new(Inoreader::default());
        fresh.start();
        let key = fresh.app().snapshot.key.clone();
        fresh.store_result(StoreResult::Loaded { key, value: None });
        fresh.store_result(StoreResult::Loaded {
            key: pending::KEY.to_owned(),
            value: None,
        });
        fresh.store_result(StoreResult::Loaded {
            key: positions::KEY.to_owned(),
            value: positions,
        });
        fresh.app_mut().articles = runner.app().articles.clone();
        fresh.app_mut().loaded = true;
        fresh
    }

    fn position_write(commands: &[Command]) -> Option<Vec<u8>> {
        commands.iter().find_map(|command| match command {
            Command::Store(StoreRequest::Save { key, value }) if key == positions::KEY => {
                Some(value.clone())
            }
            _ => None,
        })
    }

    #[test]
    fn a_reading_position_is_kept_on_back_and_restored_after_a_restart() {
        let mut runner = holding_story(STORY, 80);
        runner.action(action_id(&article_action(0)));
        let start = runner.app().book.memory().cloned().expect("open");
        // Turning a page keeps the place at once; Back keeps it again.
        let commands = runner.action(action_id(kobo_read::action::FORWARD));
        let turned = runner.app().book.memory().cloned().expect("open");
        assert_ne!(turned, start, "the page did not turn");
        let written = position_write(&commands).expect("the turn kept the position");
        runner.store_result(StoreResult::Saved {
            key: positions::KEY.to_owned(),
        });
        runner.action(ActionId::BACK);
        assert!(!runner.app().progress.failed);

        let mut fresh = restarted_with(&runner, Some(written));
        fresh.action(action_id(&article_action(0)));
        assert_eq!(fresh.app().book.memory().cloned(), Some(turned));
    }

    #[test]
    fn a_position_that_could_not_be_saved_offers_retry() {
        let mut runner = holding_story(STORY, 80);
        runner.action(action_id(&article_action(0)));
        // Opening asked for the picture's saved copy and queued a read
        // change; both are answered first, in that order.
        runner.store_result(StoreResult::Loaded {
            key: "rss-image:https://example.com/images/a.png".to_owned(),
            value: None,
        });
        runner.store_result(StoreResult::Saved {
            key: pending::KEY.to_owned(),
        });
        let commands = runner.action(action_id(kobo_read::action::FORWARD));
        assert!(position_write(&commands).is_some());
        runner.action(ActionId::BACK);
        let commands = runner.store_result(StoreResult::Denied(kobo_sdk::StoreError::NoRoom));
        assert!(runner.app().progress.failed);
        let text = screen_text(&last_screen(&commands).expect("a screen"));
        let screen = last_screen(&commands).expect("a screen");
        assert!(
            format!("{screen:?}").contains(&format!("{:?}", action_id(RETRY_SAVE))),
            "no retry action"
        );
        assert!(text.contains("Reading positions are not saved."), "{text}");
        let commands = runner.action(action_id(RETRY_SAVE));
        assert!(position_write(&commands).is_some(), "retry wrote nothing");
    }

    #[test]
    fn a_named_picture_is_fetched_once_saved_and_read_from_the_saved_copy() {
        let png = kobo_image::encode_png_grey(64, 64, &vec![128; 64 * 64]).expect("a png");
        let mut runner = holding_story(STORY, 1);
        let commands = runner.action(action_id(&article_action(0)));
        // The picture cache answers "nothing saved" before anything is fetched.
        assert!(fetches_of(&commands).is_empty());
        let key = format!("rss-image:{}", "https://example.com/images/a.png");
        let commands = runner.store_result(StoreResult::Loaded { key, value: None });
        let fetches = fetches_of(&commands);
        assert_eq!(fetches.len(), 1, "{commands:#?}");
        assert_eq!(fetches[0].1, "https://example.com/images/a.png");
        assert_eq!(fetches[0].2, None, "a picture must carry no credential");

        assert!(!format!("{:?}", runner.app().book.screen("Story")).contains("Picture"));
        let commands = runner.task_outcome(fetches[0].0, TaskOutcome::Completed(png.clone()));
        let shown = format!("{:?}", runner.app().book.screen("Story"));
        assert!(shown.contains("Picture"), "the picture is not on the page");
        let (name, bytes) = commands
            .iter()
            .find_map(|command| match command {
                Command::Store(StoreRequest::ShelfWrite { name, bytes, .. }) => {
                    Some((name.clone(), bytes.clone()))
                }
                _ => None,
            })
            .expect("the picture was saved");
        runner.store_result(StoreResult::ShelfWritten {
            name: name.clone(),
            size: u32::try_from(bytes.len()).expect("small"),
        });
        let key = format!("rss-image:{}", "https://example.com/images/a.png");
        runner.store_result(StoreResult::Saved { key: key.clone() });
        runner.action(ActionId::BACK);

        // After a restart nothing is fetched: the picture comes off the shelf.
        let mut fresh = restarted_with(&runner, None);
        let commands = fresh.action(action_id(&article_action(0)));
        assert!(fetches_of(&commands).is_empty());
        let pointer = format!("1:{}", kobo_net::sha256::hex_digest(&bytes)).into_bytes();
        let commands = fresh.store_result(StoreResult::Loaded {
            key,
            value: Some(pointer),
        });
        assert!(fetches_of(&commands).is_empty());
        let read = commands.iter().any(|command| {
            matches!(command, Command::Store(StoreRequest::ShelfRead { name: asked, .. }) if *asked == name)
        });
        assert!(read, "the saved copy was not read: {commands:#?}");
        let commands = fresh.store_result(StoreResult::ShelfRead {
            name,
            offset: 0,
            size: u32::try_from(bytes.len()).expect("small"),
            bytes,
        });
        assert!(fetches_of(&commands).is_empty(), "{commands:#?}");
        assert!(!fresh.app().pictures.failed);
    }

    #[test]
    fn an_article_without_a_url_opens_without_asking_for_pictures() {
        let mut runner = holding_story("", 1);
        let commands = runner.action(action_id(&article_action(0)));
        let asked = commands.iter().any(|command| {
            matches!(command, Command::Store(StoreRequest::Load { key }) if key.starts_with("rss-image:"))
        });
        assert!(!asked);
        assert!(fetches_of(&commands).is_empty());
    }
}
