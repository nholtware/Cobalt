# Inoreader

Read your Inoreader feeds on your Kobo, offline, and send your read marks and
stars back when Wi-Fi returns.

Inoreader is OAuth 2.0 only. The app never holds the token: the Cobalt
runtime keeps it under the credential name `inoreader` and attaches it as an
`Authorization: Bearer` header only to the exact requests this app's policy
allows. Typing a token on an e-ink keyboard is not a feature, so there is no
in-app credential editor.

Setup, from your computer:

```sh
# In Inoreader, open Preferences, Developer, and create an application
# with scope read and write. Keep its App ID and App Key on your computer only.
# Save them as ~/.config/cobalt/secrets/inoreader-client-id and
# inoreader-client-secret (mode 0600).

# Print the consent page, approve it, and save the code= value from the
# address bar as ~/.config/cobalt/secrets/inoreader-code.
apps/inoreader-client/tools/token.sh url
apps/inoreader-client/tools/token.sh mint

# Install the token under the exact name inoreader; the runtime sends it as
# an Authorization: Bearer header.
kobo secret set inoreader --device <address>

# When the app says Inoreader rejected the token:
apps/inoreader-client/tools/token.sh refresh
kobo secret set inoreader --device <address>
```

`token.sh check` reports whether the token is still accepted and how much of
Inoreader's daily quota is used; `token.sh install --sim` copies it to the
simulator.

The token expires about every 24 hours. The runtime never shows the app the
secret's value, and a refresh needs the client secret in a request body, so
the refresh happens on the computer: run `tools/token.sh refresh`, then
install the result again.

## Three tabs, one download

A sync downloads three streams: unread (30 articles), starred (15) and
recently read (15). They are merged into one saved copy, so every tab works
with the radio off. An article is shown as unread if it arrived in the unread
stream, and as read if it arrived only in the recently read stream. Starred
comes from the starred stream.

## Reading

Articles open in the shared document reader. Pictures an article names are
fetched once, without any credential, and saved for offline reading. Your
place is kept when you turn a page and on Back, and comes back after a
restart. A feed that sends only a summary says so; the public API cannot
fetch the full page behind it.

## Changes made away from Wi-Fi

- Opening an article marks it read.
- The row menu stars an article or keeps it unread.
- Every change is written down on the reader before it is sent, so nothing is
  lost if the battery dies or Wi-Fi drops.
- Changes are assignments (add a tag, remove a tag), never toggles, so a
  request that is sent twice asks for the same end state.
- Inoreader allows 100 writes a day, so changes go out one request per kind,
  with many articles in each.
- Changes go out before a sync downloads, so a sync never shows you an old
  state of something you just changed.

## Limits

- 768 KiB per reply, 60 articles per sync, 256 KiB per article body.
- Inoreader allows 100 reads and 100 writes a day on a free account; a sync
  is a small fixed number of reads.
- No full-article fetch and no archive: Inoreader has neither in its public
  API. Marking read is the nearest thing.

## Dependencies

| Dependency | Kind |
| --- | --- |
| Inoreader (`www.inoreader.com`) | Proprietary remote service, not vendored |
| `kobo-sdk` | Workspace crate: app runtime, screens, requests |
| `kobo-json`, `kobo-net` | Workspace crates: reply parsing, hashing |
| `kobo-doc`, `kobo-read`, `kobo-bookview` | Workspace crates: the shared reader, positions and pictures |
