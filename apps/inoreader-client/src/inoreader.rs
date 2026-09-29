//! Talking to Inoreader: the Unread stream request, its parser, and the
//! app's own saved-batch encoding.

use crate::pending::Change;
use kobo_json::Value;
use kobo_sdk::{Credential, Task};
use std::fmt::Write;

/// The identity of the saved batch on the reader.
pub const SNAPSHOT_IDENTITY: &str = "inoreader-articles-v1:inoreader";

/// The longest reply, and the longest saved batch, in bytes.
pub const MAX_RESPONSE: usize = 768 * 1024;
/// The most articles a saved batch holds.
pub const MAX_ARTICLES: usize = 60;
/// The longest one article body, in bytes.
pub const MAX_BODY: usize = 256 * 1024;

const ITEM_PREFIX: &str = "tag:google.com,2005:reader/item/";
const STARRED_SUFFIX: &str = "/state/com.google/starred";
/// The largest item number a saved batch can hold exactly: the batch stores
/// it as a JSON number, which the parser reads as an `f64`.
const MAX_EXACT_ITEM: u64 = 1 << 53;

/// Whether an article is still unread. The stream an item arrived in says
/// so; the item's own tags do not reliably.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Unread,
    Read,
}

impl Status {
    const fn word(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::Read => "read",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Article {
    pub id: u64,
    pub title: String,
    pub feed: String,
    pub content: String,
    pub url: String,
    pub starred: bool,
    pub status: Status,
}

/// Why a reply, or a saved batch, was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseError {
    Malformed,
    TooLarge,
    InvalidItem,
}

impl ParseError {
    /// The sentence the reader sees.
    #[must_use]
    pub const fn sentence(self) -> &'static str {
        match self {
            Self::Malformed => {
                "Inoreader did not return a valid response. Refresh the token on your computer \
                 and Sync again."
            }
            Self::TooLarge => {
                "Inoreader sent more than this Kobo can hold. Your saved articles are kept."
            }
            Self::InvalidItem => {
                "The response contains an article this Kobo cannot read. Your saved articles \
                 are kept."
            }
        }
    }
}

/// One of the three streams a sync reads, in the order it reads them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Part {
    Unread,
    Starred,
    Read,
}

impl Part {
    /// The order a sync walks the parts, and the order they merge in.
    pub const ORDER: [Self; 3] = [Self::Unread, Self::Starred, Self::Read];

    /// The exact URL, pinned here and in the policy arm in
    /// `crates/kobo-policy/src/credentials.rs`, which asserts these strings.
    #[must_use]
    pub const fn url(self) -> &'static str {
        match self {
            Self::Unread => "https://www.inoreader.com/reader/api/0/stream/contents/user/-/state/com.google/reading-list?n=30&xt=user/-/state/com.google/read&output=json",
            Self::Starred => "https://www.inoreader.com/reader/api/0/stream/contents/user/-/state/com.google/starred?n=15&output=json",
            Self::Read => "https://www.inoreader.com/reader/api/0/stream/contents/user/-/state/com.google/read?n=15&output=json",
        }
    }

    /// The status items get from the stream they arrived in.
    #[must_use]
    pub const fn status(self) -> Status {
        match self {
            Self::Unread => Status::Unread,
            Self::Starred | Self::Read => Status::Read,
        }
    }
}

/// The request for one part.
#[must_use]
pub fn request(part: Part) -> Task {
    Task::Fetch {
        url: part.url().to_owned(),
        offset: 0,
        max_bytes: u32::try_from(MAX_RESPONSE).unwrap_or(u32::MAX),
        credential: Some(Credential::bearer("inoreader")),
        headers: Vec::new(),
    }
}

/// The one write route, pinned here and in the policy.
pub const EDIT_TAG_URL: &str = "https://www.inoreader.com/reader/api/0/edit-tag";
const READ_TAG: &str = "user/-/state/com.google/read";
const STARRED_TAG: &str = "user/-/state/com.google/starred";

/// The request that asks Inoreader for the end state `change` names, for
/// every article in `ids`.
#[must_use]
pub fn edit_tag(change: Change, ids: &[u64]) -> Task {
    let (verb, tag) = match change {
        Change::Read => ("a", READ_TAG),
        Change::Unread => ("r", READ_TAG),
        Change::Star => ("a", STARRED_TAG),
        Change::Unstar => ("r", STARRED_TAG),
    };
    let mut body = format!("{verb}={tag}");
    for id in ids {
        let _ = write!(body, "&i={id}");
    }
    Task::Post {
        url: EDIT_TAG_URL.to_owned(),
        body,
        content_type: "application/x-www-form-urlencoded".to_owned(),
        credential: Some(Credential::bearer("inoreader")),
        headers: Vec::new(),
        max_bytes: 1024,
    }
}

/// Whether a reply to [`edit_tag`] is Inoreader's `OK`.
#[must_use]
pub fn is_ok(bytes: &[u8]) -> bool {
    bytes.trim_ascii() == b"OK"
}

/// Folds the articles of a later part into those already held. An item
/// already held keeps the status it arrived with first; a held item seen
/// again in the Starred stream becomes starred.
pub fn merge(held: &mut Vec<Article>, arrived: Vec<Article>) {
    for article in arrived {
        match held.iter_mut().find(|have| have.id == article.id) {
            Some(have) => have.starred |= article.starred,
            None => held.push(article),
        }
    }
}

/// The item number in a long-form item id: the sixteen hex digits after
/// `item/`, as the decimal `u64` `edit-tag` accepts.
#[must_use]
pub fn item_number(id: &str) -> Option<u64> {
    let digits = id.strip_prefix(ITEM_PREFIX)?;
    if digits.len() != 16 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let number = u64::from_str_radix(digits, 16).ok()?;
    (number != 0 && number < MAX_EXACT_ITEM).then_some(number)
}

fn has_repeated_key(value: &Value) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    fields
        .iter()
        .enumerate()
        .any(|(at, (name, _))| fields[..at].iter().any(|(earlier, _)| earlier == name))
}

fn first_href(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .index(0)?
        .get("href")?
        .as_str()
        .map(str::to_owned)
}

fn text_of(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Reads a stream reply into articles, every one marked with the status of
/// `part`, and starred when `part` is the Starred stream.
///
/// # Errors
///
/// A [`ParseError`] for anything that is not a well-formed stream, or that
/// is larger than this Kobo keeps.
pub fn parse_stream(bytes: &[u8], part: Part) -> Result<Vec<Article>, ParseError> {
    if bytes.len() > MAX_RESPONSE {
        return Err(ParseError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ParseError::Malformed)?;
    let reply = kobo_json::parse(text).map_err(|_| ParseError::Malformed)?;
    if has_repeated_key(&reply) {
        return Err(ParseError::Malformed);
    }
    let items = reply
        .get("items")
        .and_then(Value::as_array)
        .ok_or(ParseError::Malformed)?;
    if items.len() > MAX_ARTICLES {
        return Err(ParseError::TooLarge);
    }
    let mut articles: Vec<Article> = Vec::with_capacity(items.len());
    for item in items {
        if has_repeated_key(item) {
            return Err(ParseError::Malformed);
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .and_then(item_number)
            .ok_or(ParseError::InvalidItem)?;
        if articles.iter().any(|article| article.id == id) {
            return Err(ParseError::InvalidItem);
        }
        let content = item
            .get("summary")
            .and_then(|summary| summary.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if content.len() > MAX_BODY {
            return Err(ParseError::TooLarge);
        }
        let title = text_of(item, "title");
        let feed = item
            .get("origin")
            .and_then(|origin| origin.get("title"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("Feed");
        let starred = part == Part::Starred
            || item
                .get("categories")
                .and_then(Value::as_array)
                .is_some_and(|tags| {
                    tags.iter()
                        .filter_map(Value::as_str)
                        .any(|tag| tag.ends_with(STARRED_SUFFIX))
                });
        articles.push(Article {
            id,
            title: if title.is_empty() {
                "Untitled".to_owned()
            } else {
                title
            },
            feed: feed.to_owned(),
            content: content.to_owned(),
            url: first_href(item, "canonical")
                .or_else(|| first_href(item, "alternate"))
                .unwrap_or_default(),
            starred,
            status: part.status(),
        });
    }
    Ok(articles)
}

/// The batch the app saves: its own JSON, not Inoreader's reply.
#[must_use]
pub fn encode_saved(articles: &[Article]) -> Vec<u8> {
    let mut out = String::from("{\"articles\":[");
    for (at, article) in articles.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        out.push_str("{\"id\":");
        out.push_str(&article.id.to_string());
        for (key, value) in [
            ("title", &article.title),
            ("feed", &article.feed),
            ("content", &article.content),
            ("url", &article.url),
        ] {
            out.push_str(",\"");
            out.push_str(key);
            out.push_str("\":");
            kobo_json::escape_into(value, &mut out);
        }
        out.push_str(",\"starred\":");
        out.push_str(if article.starred { "true" } else { "false" });
        out.push_str(",\"status\":\"");
        out.push_str(article.status.word());
        out.push_str("\"}");
    }
    out.push_str("]}");
    out.into_bytes()
}

/// Reads a batch written by [`encode_saved`].
///
/// # Errors
///
/// A [`ParseError`] for a batch that is not exactly that shape.
pub fn parse_saved(bytes: &[u8]) -> Result<Vec<Article>, ParseError> {
    if bytes.len() > MAX_RESPONSE {
        return Err(ParseError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ParseError::Malformed)?;
    let saved = kobo_json::parse(text).map_err(|_| ParseError::Malformed)?;
    let entries = saved
        .get("articles")
        .and_then(Value::as_array)
        .ok_or(ParseError::Malformed)?;
    if entries.len() > MAX_ARTICLES {
        return Err(ParseError::TooLarge);
    }
    let mut articles = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = entry
            .get("id")
            .and_then(Value::as_f64)
            .filter(|number| number.fract() == 0.0 && *number >= 1.0)
            .ok_or(ParseError::InvalidItem)?;
        // Exact: `encode_saved` never writes above `MAX_EXACT_ITEM`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let id = id as u64;
        if id >= MAX_EXACT_ITEM {
            return Err(ParseError::InvalidItem);
        }
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(ParseError::InvalidItem)
        };
        let content = field("content")?;
        if content.len() > MAX_BODY {
            return Err(ParseError::TooLarge);
        }
        let status = match entry.get("status").and_then(Value::as_str) {
            Some("unread") => Status::Unread,
            Some("read") => Status::Read,
            _ => return Err(ParseError::InvalidItem),
        };
        articles.push(Article {
            id,
            title: field("title")?,
            feed: field("feed")?,
            content,
            url: field("url")?,
            starred: entry
                .get("starred")
                .and_then(Value::as_bool)
                .ok_or(ParseError::InvalidItem)?,
            status,
        });
    }
    Ok(articles)
}

#[cfg(test)]
mod tests {
    use super::{
        edit_tag, encode_saved, is_ok, item_number, merge, parse_saved, parse_stream, request,
        Article, ParseError, Part, Status, MAX_RESPONSE,
    };
    use crate::pending::Change;
    use kobo_sdk::{Credential, Task};

    #[test]
    fn each_change_asks_for_its_own_end_state_in_one_body() {
        let cases = [
            (Change::Read, "a=user/-/state/com.google/read&i=7&i=9"),
            (Change::Unread, "r=user/-/state/com.google/read&i=7&i=9"),
            (Change::Star, "a=user/-/state/com.google/starred&i=7&i=9"),
            (Change::Unstar, "r=user/-/state/com.google/starred&i=7&i=9"),
        ];
        for (change, expected) in cases {
            let task = edit_tag(change, &[7, 9]);
            assert!(task.is_sendable());
            let Task::Post {
                url,
                body,
                content_type,
                credential,
                headers,
                max_bytes,
            } = task
            else {
                panic!("not a POST")
            };
            assert_eq!(url, "https://www.inoreader.com/reader/api/0/edit-tag");
            assert_eq!(body, expected);
            assert_eq!(content_type, "application/x-www-form-urlencoded");
            assert_eq!(credential, Some(Credential::bearer("inoreader")));
            assert!(headers.is_empty());
            assert_eq!(max_bytes, 1024);
        }
    }

    #[test]
    fn only_ok_confirms_a_change() {
        assert!(is_ok(b"OK\n"));
        assert!(is_ok(b"OK"));
        assert!(!is_ok(b"{}"));
        assert!(!is_ok(b""));
    }

    const ITEM: &str = r#"{
        "id":"tag:google.com,2005:reader/item/0000000bcaa77f5b",
        "title":"A \"quoted\" headline",
        "summary":{"direction":"ltr","content":"<p>Body <em>text</em></p>"},
        "canonical":[{"href":"https://example.com/a"}],
        "alternate":[{"href":"https://example.com/alt","type":"text/html"}],
        "origin":{"streamId":"feed/https://example.com/rss","title":"Example Feed"},
        "categories":["user/1006150148/state/com.google/reading-list",
                      "user/1006150148/state/com.google/starred",
                      "user/1006150148/label/Tech"]
    }"#;

    fn stream(items: &str) -> Vec<u8> {
        format!(r#"{{"id":"x","items":[{items}]}}"#).into_bytes()
    }

    #[test]
    fn every_part_pins_its_url_secret_limit_and_headers() {
        let urls: Vec<&str> = Part::ORDER.iter().map(|part| part.url()).collect();
        assert_eq!(urls.len(), 3);
        for (at, part) in Part::ORDER.into_iter().enumerate() {
            assert_eq!(urls.iter().filter(|url| **url == urls[at]).count(), 1);
            let work = request(part);
            let Task::Fetch {
                url,
                offset,
                max_bytes,
                credential,
                headers,
            } = &work
            else {
                panic!("not a fetch")
            };
            assert_eq!(url, part.url());
            assert_eq!(*offset, 0);
            assert_eq!(*max_bytes as usize, MAX_RESPONSE);
            assert_eq!(credential.as_ref(), Some(&Credential::bearer("inoreader")));
            assert!(headers.is_empty());
            assert!(work.is_sendable());
        }
    }

    #[test]
    fn an_item_from_the_starred_stream_is_starred_without_the_category() {
        let item = r#"{"id":"tag:google.com,2005:reader/item/0000000000000003"}"#;
        let starred = parse_stream(&stream(item), Part::Starred).expect("parses");
        assert!(starred[0].starred);
        assert_eq!(starred[0].status, Status::Read);
        let read = parse_stream(&stream(item), Part::Read).expect("parses");
        assert!(!read[0].starred);
    }

    #[test]
    fn a_held_item_keeps_its_first_status_and_gains_a_star() {
        let item = r#"{"id":"tag:google.com,2005:reader/item/0000000000000003"}"#;
        let mut held = parse_stream(&stream(item), Part::Unread).expect("parses");
        merge(
            &mut held,
            parse_stream(&stream(item), Part::Starred).expect("parses"),
        );
        merge(
            &mut held,
            parse_stream(&stream(item), Part::Read).expect("parses"),
        );
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].status, Status::Unread);
        assert!(held[0].starred);
    }

    #[test]
    fn the_item_number_is_the_hex_tail() {
        let id = |tail: &str| format!("tag:google.com,2005:reader/item/{tail}");
        assert_eq!(item_number(&id("0000000bcaa77f5b")), Some(50_644_615_003));
        assert_eq!(item_number("0000000bcaa77f5b"), None);
        assert_eq!(item_number(&id("000000bcaa77f5b")), None);
        assert_eq!(item_number(&id("00000000bcaa77f5b")), None);
        assert_eq!(item_number(&id("0000000bcaa77f5g")), None);
        assert_eq!(item_number(&id("0000000000000000")), None);
    }

    #[test]
    fn a_real_shaped_item_reads_into_an_article() {
        let articles = parse_stream(&stream(ITEM), Part::Unread).expect("parses");
        assert_eq!(
            articles,
            vec![Article {
                id: 50_644_615_003,
                title: "A \"quoted\" headline".to_owned(),
                feed: "Example Feed".to_owned(),
                content: "<p>Body <em>text</em></p>".to_owned(),
                url: "https://example.com/a".to_owned(),
                starred: true,
                status: Status::Unread,
            }]
        );
    }

    #[test]
    fn a_missing_summary_and_missing_links_do_not_reject_an_item() {
        let item = r#"{"id":"tag:google.com,2005:reader/item/0000000000000001","title":""}"#;
        let articles = parse_stream(&stream(item), Part::Read).expect("parses");
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Untitled");
        assert_eq!(articles[0].feed, "Feed");
        assert_eq!(articles[0].content, "");
        assert_eq!(articles[0].url, "");
        assert!(!articles[0].starred);
        assert_eq!(articles[0].status, Status::Read);

        let alternate = r#"{"id":"tag:google.com,2005:reader/item/0000000000000002",
            "alternate":[{"href":"https://example.com/alt"}]}"#;
        let articles = parse_stream(&stream(alternate), Part::Unread).expect("parses");
        assert_eq!(articles[0].url, "https://example.com/alt");
    }

    #[test]
    fn a_reply_that_is_not_a_stream_is_an_error() {
        assert_eq!(parse_stream(br#"{"items":[]}"#, Part::Unread), Ok(vec![]));
        for body in [
            &b"<html>Sign in</html>"[..],
            &b"{}"[..],
            &br#"{"items":{}}"#[..],
            &[0xff, 0xfe][..],
            &br#"{"items":[],"items":[]}"#[..],
        ] {
            assert_eq!(
                parse_stream(body, Part::Unread),
                Err(ParseError::Malformed),
                "{body:?}"
            );
        }
        let bad_id = r#"{"id":"item/1"}"#;
        let twice = format!("{ITEM},{ITEM}");
        let repeated =
            r#"{"id":"tag:google.com,2005:reader/item/0000000000000001","title":"a","title":"b"}"#;
        assert_eq!(
            parse_stream(br#"{"items":[{}]}"#, Part::Unread),
            Err(ParseError::InvalidItem)
        );
        assert_eq!(
            parse_stream(&stream(bad_id), Part::Unread),
            Err(ParseError::InvalidItem)
        );
        assert_eq!(
            parse_stream(&stream(&twice), Part::Unread),
            Err(ParseError::InvalidItem)
        );
        assert_eq!(
            parse_stream(&stream(repeated), Part::Unread),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse_stream(&vec![b' '; MAX_RESPONSE + 1], Part::Unread),
            Err(ParseError::TooLarge)
        );
    }

    #[test]
    fn a_batch_round_trips_with_a_quote_and_markup() {
        let articles = vec![
            Article {
                id: 7,
                title: "He said \"hi\"".to_owned(),
                feed: "Feed".to_owned(),
                content: "<p>a <em>b</em></p>\n".to_owned(),
                url: "https://example.com/".to_owned(),
                starred: true,
                status: Status::Unread,
            },
            Article {
                id: 50_644_615_003,
                title: "Two".to_owned(),
                feed: "Other".to_owned(),
                content: String::new(),
                url: String::new(),
                starred: false,
                status: Status::Read,
            },
        ];
        assert_eq!(parse_saved(&encode_saved(&articles)), Ok(articles));
        assert_eq!(parse_saved(b"{}"), Err(ParseError::Malformed));
        assert_eq!(parse_saved(b"garbage"), Err(ParseError::Malformed));
    }
}
