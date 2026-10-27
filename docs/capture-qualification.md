# YouTube capture qualification

Three separate claims require different evidence:

1. The Obsidian extension is installed and captures text in the user's browser.
2. An acquired transcript converts without loss and reads back through the service.
3. The acquired track covers the recording accurately and completely.

An in-app-browser `exportYouTubeTranscript` result is not a test of the Obsidian
extension. A template JSON on a server does not prove browser installation or
that `{{transcript}}` produced text. If capture fails, record that result; never
substitute a description or headings and call it a transcript.

Import `templates/obsidian-web-clipper-youtube.json` manually in Web Clipper. Its
`page_language` uses Obsidian's page variable; `caption_language` and
`caption_type` remain unknown until the actual track is checked. Marked legacy
captures using `language` do not attest that field as caption language. Ordinary
Markdown with explicitly supplied language retains its existing behavior.

The Rust importer rejects marked YouTube captures with no timestamped segments,
including the fixed template title after an empty transcript substitution.
Generic Markdown documents remain supported independently of that rule.

## Verify an existing private library

The read-only Rust example takes an absolute data root, the private query socket
and an expected archive-version count:

```text
cargo run --locked -p kf-app --example verify_library -- DATA_ROOT SOCKET 5
```

It rehashes raw files, reparses every segment, reproduces each complete structured
record and Markdown byte-for-byte, then reads each current document through the
real service with a pinned version until explicit EOF. It prints metadata and
integrity results, without transcript text, Google calls, writes or inference.
An obsolete archived version that is no longer current must be examined
separately; the service check intentionally refuses a different current version.

Success certifies preservation relative to the acquired source. It does not
certify transcription against audio, a new acquisition, or an extension that
was never operated. Keep capture attempts and private corpus reports outside Git.

Official reference: [Obsidian Web Clipper variables](https://help.obsidian.md/web-clipper/variables).
