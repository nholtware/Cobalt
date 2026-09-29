//! The changes a reader has made that Inoreader has not confirmed.
//!
//! Every change is a local fact first: it is written to the store before it
//! is sent, and it leaves the queue only when Inoreader says `OK`. Every body
//! sent asks for an end state, so a change whose reply went missing can be
//! sent again without harm.

use kobo_sdk::{Context, StoreResult};
use std::fmt::Write;

/// The store key the queue is kept under.
pub const KEY: &str = "read-actions";
/// The most ids one request carries.
pub const MAX_BATCH: usize = 50;
/// The longest encoded queue, in bytes.
pub const MAX_BYTES: usize = 64 * 1024;
/// The most changes the queue holds.
pub const MAX_ENTRIES: usize = 500;

const HEADER: &str = "inoreader-changes-v1";

/// One thing the reader did to one article.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    Read,
    Unread,
    Star,
    Unstar,
}

impl Change {
    const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Unread => "unread",
            Self::Star => "star",
            Self::Unstar => "unstar",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [Self::Read, Self::Unread, Self::Star, Self::Unstar]
            .into_iter()
            .find(|change| change.name() == name)
    }

    /// The change that undoes this one.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Read => Self::Unread,
            Self::Unread => Self::Read,
            Self::Star => Self::Unstar,
            Self::Unstar => Self::Star,
        }
    }
}

#[derive(Default)]
pub struct Pending {
    queue: Vec<(u64, Change)>,
    loaded: bool,
    failed: bool,
    /// The batch that has been sent and not yet answered.
    in_flight: Option<(Change, Vec<u64>)>,
    /// The bytes of the save that is out.
    saving: Option<Vec<u8>>,
    /// The bytes the store last acknowledged.
    acknowledged: Option<Vec<u8>>,
}

fn decode(bytes: &[u8]) -> Option<Vec<(u64, Change)>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let mut queue = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (id, name) = line.split_once('\t')?;
        if id.is_empty()
            || id.len() > 16
            || !id.bytes().all(|byte| byte.is_ascii_digit())
            || queue.len() >= MAX_ENTRIES
        {
            return None;
        }
        let id: u64 = id.parse().ok()?;
        queue.push((id, Change::from_name(name)?));
    }
    Some(queue)
}

impl Pending {
    /// Asks the store for the queue.
    pub fn start(context: &mut Context) {
        context.store().load(KEY);
    }

    fn encode(&self) -> Vec<u8> {
        let mut text = format!("{HEADER}\n");
        for (id, change) in &self.queue {
            let _ = writeln!(text, "{id}\t{}", change.name());
        }
        text.into_bytes()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue could not be read, or could not be kept.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }

    #[must_use]
    pub const fn loaded(&self) -> bool {
        self.loaded
    }

    /// Every change waiting for one article, oldest first.
    pub fn changes_to(&self, id: u64) -> impl Iterator<Item = Change> + '_ {
        self.queue
            .iter()
            .filter(move |(waiting, _)| *waiting == id)
            .map(|(_, change)| *change)
    }

    /// Whether the entry at `at` belongs to the batch in flight: the oldest
    /// entry of its kind for its article.
    fn is_in_flight(&self, at: usize) -> bool {
        let Some((kind, ids)) = &self.in_flight else {
            return false;
        };
        let (id, change) = self.queue[at];
        change == *kind && ids.contains(&id) && !self.queue[..at].contains(&(id, change))
    }

    /// Queues a change. `false` when it could not be kept: the queue has not
    /// been loaded yet, or is full.
    pub fn push(&mut self, context: &mut Context, id: u64, change: Change) -> bool {
        if !self.loaded || self.queue.len() >= MAX_ENTRIES {
            self.failed = true;
            return false;
        }
        let waiting = |this: &Self, wanted: Change| {
            (0..this.queue.len())
                .rev()
                .find(|&at| this.queue[at] == (id, wanted) && !this.is_in_flight(at))
        };
        if let Some(at) = waiting(self, change.opposite()) {
            self.queue.remove(at);
        } else if waiting(self, change).is_none() {
            self.queue.push((id, change));
        }
        self.save(context);
        true
    }

    /// The next request to send: the head's kind, and every waiting change
    /// of that kind. `None` while one is out or nothing waits.
    #[must_use]
    pub fn next_batch(&self) -> Option<(Change, Vec<u64>)> {
        if self.in_flight.is_some() {
            return None;
        }
        let kind = self.queue.first()?.1;
        let mut ids = Vec::new();
        for (id, change) in &self.queue {
            if *change == kind && !ids.contains(id) && ids.len() < MAX_BATCH {
                ids.push(*id);
            }
        }
        Some((kind, ids))
    }

    /// Records that this batch has been sent.
    pub fn begin(&mut self, kind: Change, ids: Vec<u64>) {
        self.in_flight = Some((kind, ids));
    }

    /// Inoreader confirmed the batch: its entries leave the queue.
    pub fn applied(&mut self, context: &mut Context, kind: Change, ids: &[u64]) {
        for id in ids {
            if let Some(at) = self.queue.iter().position(|entry| *entry == (*id, kind)) {
                self.queue.remove(at);
            }
        }
        self.in_flight = None;
        self.save(context);
    }

    /// The reply never came. The batch may already have been applied, and
    /// asking again for the same end state costs nothing, so it stays.
    pub fn unresolved(&mut self) {
        self.in_flight = None;
    }

    /// Writes the queue unless a write is out or the store already holds it.
    fn save(&mut self, context: &mut Context) {
        if !self.loaded || self.saving.is_some() {
            return;
        }
        let bytes = self.encode();
        if bytes.len() > MAX_BYTES {
            self.failed = true;
            return;
        }
        if self.acknowledged.as_deref() == Some(bytes.as_slice()) {
            return;
        }
        context.store().save(KEY, bytes.clone());
        self.saving = Some(bytes);
    }

    /// Reloads when the queue never loaded, otherwise writes it again.
    pub fn retry(&mut self, context: &mut Context) {
        if self.loaded {
            self.failed = false;
            self.saving = None;
            self.acknowledged = None;
            self.save(context);
        } else {
            self.failed = false;
            Self::start(context);
        }
    }

    /// Takes the store's answer to a load or a save of [`KEY`].
    pub fn stored(&mut self, context: &mut Context, result: &StoreResult) {
        match result {
            StoreResult::Loaded { value, .. } => {
                self.loaded = true;
                match value {
                    None => self.queue.clear(),
                    Some(bytes) => {
                        if let Some(queue) = decode(bytes) {
                            self.queue = queue;
                            self.acknowledged = Some(bytes.clone());
                        } else {
                            self.queue.clear();
                            self.failed = true;
                        }
                    }
                }
            }
            StoreResult::Saved { .. } => {
                self.acknowledged = self.saving.take();
                self.save(context);
            }
            _ => {
                self.saving = None;
                self.failed = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, Pending, KEY, MAX_BATCH};
    use kobo_sdk::{Command, Context, StoreError, StoreRequest, StoreResult};

    fn loaded(value: Option<&[u8]>) -> (Pending, Context) {
        let mut pending = Pending::default();
        let mut context = Context::default();
        pending.stored(
            &mut context,
            &StoreResult::Loaded {
                key: KEY.to_owned(),
                value: value.map(<[u8]>::to_vec),
            },
        );
        (pending, context)
    }

    fn saves(context: &mut Context) -> usize {
        context
            .take_commands()
            .iter()
            .filter(|command| matches!(command, Command::Store(StoreRequest::Save { .. })))
            .count()
    }

    fn saved() -> StoreResult {
        StoreResult::Saved {
            key: KEY.to_owned(),
        }
    }

    #[test]
    fn a_queue_without_the_format_line_is_refused() {
        for bytes in [&b"7,9"[..], b"not a queue"] {
            let (pending, _) = loaded(Some(bytes));
            assert!(pending.failed());
            assert_eq!(pending.len(), 0);
        }
        let (pending, _) = loaded(None);
        assert!(!pending.failed());
        assert_eq!(pending.len(), 0);
        let (pending, _) = loaded(Some(b"inoreader-changes-v1\n7\tstar\n9\tread\n"));
        assert!(!pending.failed());
        assert_eq!(pending.len(), 2);
        let (pending, _) = loaded(Some(b"inoreader-changes-v1\n7\tdance\n"));
        assert!(pending.failed());
    }

    #[test]
    fn nothing_is_queued_before_the_store_has_answered() {
        let mut pending = Pending::default();
        let mut context = Context::default();
        assert!(!pending.push(&mut context, 7, Change::Read));
        assert!(pending.failed());
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn undoing_a_waiting_change_removes_it_rather_than_queueing_its_opposite() {
        let (mut pending, mut context) = loaded(None);
        pending.push(&mut context, 7, Change::Star);
        pending.push(&mut context, 7, Change::Unstar);
        assert_eq!(pending.len(), 0);
        pending.push(&mut context, 7, Change::Read);
        pending.push(&mut context, 7, Change::Read);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn a_change_being_sent_is_never_collapsed_by_a_later_one() {
        let (mut pending, mut context) = loaded(None);
        pending.push(&mut context, 7, Change::Star);
        let (kind, ids) = pending.next_batch().expect("a batch");
        pending.begin(kind, ids.clone());
        assert!(pending.next_batch().is_none());
        pending.push(&mut context, 7, Change::Unstar);
        assert_eq!(pending.len(), 2);
        pending.applied(&mut context, kind, &ids);
        assert_eq!(pending.next_batch(), Some((Change::Unstar, vec![7])));
    }

    #[test]
    fn a_lost_reply_leaves_the_change_at_the_head() {
        let (mut pending, mut context) = loaded(None);
        pending.push(&mut context, 7, Change::Read);
        pending.push(&mut context, 8, Change::Star);
        let (kind, ids) = pending.next_batch().expect("a batch");
        pending.begin(kind, ids);
        pending.unresolved();
        assert_eq!(pending.next_batch(), Some((Change::Read, vec![7])));
        pending.applied(&mut context, Change::Read, &[7]);
        assert_eq!(pending.next_batch(), Some((Change::Star, vec![8])));
    }

    #[test]
    fn the_queue_is_written_once_and_again_only_after_acknowledgement() {
        let (mut pending, mut context) = loaded(None);
        pending.push(&mut context, 7, Change::Read);
        assert_eq!(saves(&mut context), 1);
        pending.push(&mut context, 8, Change::Read);
        assert_eq!(saves(&mut context), 0, "a second write went out");
        pending.stored(&mut context, &saved());
        let commands = context.take_commands();
        let written: Vec<&Vec<u8>> = commands
            .iter()
            .filter_map(|command| match command {
                Command::Store(StoreRequest::Save { value, .. }) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(written.len(), 1);
        let text = String::from_utf8(written[0].clone()).expect("text");
        assert!(
            text.contains("7\tread") && text.contains("8\tread"),
            "{text}"
        );
        pending.stored(&mut context, &StoreResult::Denied(StoreError::NoRoom));
        assert!(pending.failed());
    }

    #[test]
    fn a_queue_the_store_already_holds_is_not_written_again() {
        let (mut pending, mut context) = loaded(Some(b"inoreader-changes-v1\n7\tread\n"));
        pending.push(&mut context, 7, Change::Read);
        assert_eq!(saves(&mut context), 0);
    }

    #[test]
    fn a_batch_carries_every_waiting_change_of_the_heads_kind() {
        let (mut pending, mut context) = loaded(None);
        for (id, change) in [
            (1, Change::Read),
            (2, Change::Star),
            (3, Change::Read),
            (4, Change::Read),
        ] {
            pending.push(&mut context, id, change);
        }
        assert_eq!(pending.next_batch(), Some((Change::Read, vec![1, 3, 4])));
        pending.applied(&mut context, Change::Read, &[1, 3, 4]);
        assert_eq!(pending.next_batch(), Some((Change::Star, vec![2])));
    }

    #[test]
    fn a_batch_stops_at_fifty_ids() {
        let (mut pending, mut context) = loaded(None);
        for id in 1..=60 {
            pending.push(&mut context, id, Change::Read);
        }
        let (_, ids) = pending.next_batch().expect("a batch");
        assert_eq!(ids.len(), MAX_BATCH);
        assert_eq!(ids[0], 1);
    }

    #[test]
    fn retry_reloads_a_queue_that_never_loaded_and_rewrites_one_that_did() {
        let mut pending = Pending::default();
        let mut context = Context::default();
        pending.stored(&mut context, &StoreResult::Denied(StoreError::Unwritable));
        assert!(pending.failed());
        pending.retry(&mut context);
        assert!(!pending.failed());
        assert!(context
            .take_commands()
            .iter()
            .any(|command| matches!(command, Command::Store(StoreRequest::Load { .. }))));

        let (mut pending, mut context) = loaded(None);
        pending.push(&mut context, 7, Change::Read);
        pending.stored(&mut context, &StoreResult::Denied(StoreError::NoRoom));
        let _ = context.take_commands();
        pending.retry(&mut context);
        assert!(!pending.failed());
        assert_eq!(saves(&mut context), 1);
    }
}
