Plex JSON fixtures.

- `*.json`: hand-written, following the documented response shapes. They cover
  edge cases a single server doesn't show (numbers sent as strings, Windows
  paths, a track without media). `lyrics.json` has the shape of a real
  LyricFind stream (`/library/streams/{id}` as JSON), with made-up lines.
- `recorded/*.json`: responses from a real Plex Media Server (1.43.4), fetched
  as the backend fetches them (`includeGuids=1`) and sanitized: paths moved
  under `/music`, `librarySectionUUID`, `machineIdentifier` and `uuid` dropped,
  summaries replaced, and only the music section kept.
