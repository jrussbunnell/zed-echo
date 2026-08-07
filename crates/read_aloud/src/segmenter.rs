use markdown::{
    ParsedMarkdown,
    parser::{MarkdownEvent, MarkdownTag},
};
use std::ops::Range;

/// A single spoken unit: one sentence of assistant prose.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// Byte range in the original markdown source. Drives both click-mapping
    /// and highlighting, so it must stay in original-source coordinates even
    /// though `spoken_text` has had markup stripped.
    pub source_range: Range<usize>,
    pub spoken_text: String,
    /// For each byte of `spoken_text`, the source byte it came from. Spoken
    /// text is derived from the source but not identical (markup stripped,
    /// substituted text), so word-level highlighting needs this to map a
    /// range of `spoken_text` back to a highlightable source range.
    pub(crate) spoken_origins: Vec<usize>,
}

impl Utterance {
    /// Maps a byte range of `spoken_text` back to a byte range in the
    /// original markdown source.
    pub fn source_range_for_spoken(&self, spoken: Range<usize>) -> Option<Range<usize>> {
        if spoken.start >= spoken.end {
            return None;
        }
        let &start = self.spoken_origins.get(spoken.start)?;
        let &last = self.spoken_origins.get(spoken.end - 1)?;
        Some(start..last + 1)
    }
}

/// Abbreviations that end in a period but do not end a sentence.
const ABBREVIATIONS: &[&str] = &[
    "e.g.", "i.e.", "etc.", "vs.", "cf.", "Dr.", "Mr.", "Mrs.", "Ms.", "Prof.", "St.", "approx.",
    "Fig.", "No.",
];

/// A run of spoken text paired with where it came from in the source.
struct TextRun {
    source_range: Range<usize>,
    text: String,
}

/// `message_complete` is `false` while an assistant response is still
/// streaming in and `true` once it has finished, so the caller can decide
/// whether the very last bit of content is allowed to be spoken even though
/// nothing terminates it. See `flush` for how that decision is made.
pub fn segment(parsed: &ParsedMarkdown, message_complete: bool) -> Vec<Utterance> {
    let source = parsed.source();
    let events = parsed.events();
    // `pulldown_cmark` always closes every tag it opens, even for a
    // truncated document (an unclosed code fence still gets its `End` and
    // `RootEnd`), so "the parser closed this block" is not evidence that the
    // block is finished. What *is* evidence: whether more content was parsed
    // after it. Streaming only ever appends to the end of the document, so
    // once something follows a block, that block is pinned and can never
    // change again. Only the block touching this position — the tail of the
    // document — might still grow on the next parse.
    let last_content_end = events
        .iter()
        .filter_map(|(range, event)| {
            matches!(
                event,
                MarkdownEvent::Text
                    | MarkdownEvent::Code
                    | MarkdownEvent::SubstitutedText(_)
                    | MarkdownEvent::SubstitutedCode(_)
            )
            .then_some(range.end)
        })
        .max();

    let mut utterances = Vec::new();
    let mut tag_stack: Vec<MarkdownTag> = Vec::new();
    let mut runs: Vec<TextRun> = Vec::new();

    for (range, event) in events.iter() {
        match event {
            MarkdownEvent::Start(tag) => {
                if starts_spoken_block(tag) {
                    flush(
                        &mut runs,
                        &mut utterances,
                        last_content_end,
                        message_complete,
                    );
                }
                tag_stack.push(tag.clone());
            }
            MarkdownEvent::End(_) => {
                let ended = tag_stack.pop();
                if ended.as_ref().is_some_and(|tag| starts_spoken_block(tag)) {
                    flush(
                        &mut runs,
                        &mut utterances,
                        last_content_end,
                        message_complete,
                    );
                }
            }
            MarkdownEvent::RootEnd(_) => flush(
                &mut runs,
                &mut utterances,
                last_content_end,
                message_complete,
            ),
            MarkdownEvent::Text => {
                if !is_muted(&tag_stack) {
                    if let Some(text) = source.get(range.clone()) {
                        push_prose_runs(&mut runs, range.clone(), text);
                    }
                }
            }
            MarkdownEvent::Code => {
                if !is_muted(&tag_stack) {
                    if let Some(content) = source.get(range.clone()) {
                        if let Some(text) = spoken_inline_code(content) {
                            runs.push(TextRun {
                                source_range: range.clone(),
                                text,
                            });
                        }
                    }
                }
            }
            MarkdownEvent::SubstitutedText(text) | MarkdownEvent::SubstitutedCode(text) => {
                if !is_muted(&tag_stack) {
                    runs.push(TextRun {
                        source_range: range.clone(),
                        text: text.clone(),
                    });
                }
            }
            MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => {
                if !is_muted(&tag_stack) && !runs.is_empty() {
                    runs.push(TextRun {
                        source_range: range.clone(),
                        text: " ".to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    utterances
}

/// Root-level blocks whose text is prose worth speaking.
fn starts_spoken_block(tag: &MarkdownTag) -> bool {
    matches!(
        tag,
        MarkdownTag::Paragraph | MarkdownTag::Heading { .. } | MarkdownTag::Item
    )
}

/// Pushes a plain-text run, split around tokens no human would read aloud —
/// commit-hash-like hex strings and bare URLs. The surrounding text becomes
/// separate runs, so the spoken→source origin map stays exact: the skipped
/// bytes never gain a word highlight, while the sentence keeps flowing around
/// them and (mid-sentence) the sentence wash still covers them.
fn push_prose_runs(runs: &mut Vec<TextRun>, source_range: Range<usize>, text: &str) {
    /// Punctuation hugging a token — "(aa12de9)", "aa12de9." — stays spoken:
    /// dropping a sentence terminator along with the token would merge
    /// sentences.
    const LEADING_EDGE: &[char] = &['(', '[', '{', '"', '\'', '<'];
    const TRAILING_EDGE: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '>'];

    let mut emitted_until = 0;
    for token in text.split_whitespace() {
        let token_start = token.as_ptr() as usize - text.as_ptr() as usize;
        let core = token.trim_start_matches(LEADING_EDGE);
        let core_start = token_start + (token.len() - core.len());
        let core = core.trim_end_matches(TRAILING_EDGE);
        let skip = !core.is_empty()
            && (is_hash_like(core) || core.starts_with("http://") || core.starts_with("https://"));
        if !skip {
            continue;
        }
        if emitted_until < core_start {
            runs.push(TextRun {
                source_range: source_range.start + emitted_until..source_range.start + core_start,
                text: text[emitted_until..core_start].to_string(),
            });
        }
        emitted_until = core_start + core.len();
    }
    if emitted_until < text.len() {
        runs.push(TextRun {
            source_range: source_range.start + emitted_until..source_range.end,
            text: text[emitted_until..].to_string(),
        });
    }
}

/// How an inline code span should sound, if at all. `None` skips the span.
/// The bar is "would a human reading this aloud say the token, or gesture at
/// it?" — names get spoken (with `_`/`-` as word separators), while paths,
/// expressions, flags, hashes, and URLs get gestured at.
fn spoken_inline_code(content: &str) -> Option<String> {
    /// Substrings that mark a span as code to gesture at, not prose to say.
    const CODE_SIGNALS: &[&str] = &["/", "\\", "::", "(", ")", "=", "\"", "'", "`"];
    /// A token this long without a space is an identifier nobody says aloud.
    const MAX_TOKEN_LENGTH: usize = 20;
    /// More words than this reads as a code phrase, not a name.
    const MAX_WORDS: usize = 3;

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    if CODE_SIGNALS.iter().any(|signal| trimmed.contains(signal)) {
        return None;
    }
    // `player.rs:250`-style line suffixes.
    let bytes = trimmed.as_bytes();
    if bytes
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b':' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit))
    {
        return None;
    }
    for token in trimmed.split_whitespace() {
        if token.len() >= MAX_TOKEN_LENGTH
            || token.starts_with('-') // CLI flags: -p, --foo
            || is_hash_like(token)
            || looks_file_like(token)
        {
            return None;
        }
    }
    let separated: String = content
        .chars()
        .map(|character| {
            if matches!(character, '_' | '-') {
                ' '
            } else {
                character
            }
        })
        .collect();
    // `_`/`-` are 1:1 with the space replacing them, so the spoken bytes stay
    // aligned with the source bytes for the origin map.
    (separated.split_whitespace().count() <= MAX_WORDS).then_some(separated)
}

/// Commit-SHA-shaped: a long run of pure hex. Requiring both a digit and a
/// letter keeps real words ("defaced") and plain numbers ("1234567") spoken.
fn is_hash_like(token: &str) -> bool {
    token.len() >= 7
        && token.chars().all(|character| character.is_ascii_hexdigit())
        && token.chars().any(|character| character.is_ascii_digit())
        && token
            .chars()
            .any(|character| character.is_ascii_alphabetic())
}

/// `main.rs`, `.gitignore`, `settings.json` — a dot wired into a token the
/// way file names have them.
fn looks_file_like(token: &str) -> bool {
    if token.starts_with('.') && token.len() > 1 {
        return true;
    }
    let Some(dot) = token.rfind('.') else {
        return false;
    };
    let extension = &token[dot + 1..];
    token[..dot]
        .chars()
        .next_back()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && !extension.is_empty()
        && extension.len() <= 5
        && extension.starts_with(|character: char| character.is_ascii_alphabetic())
        && extension
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

/// True when any enclosing tag makes the text non-prose.
fn is_muted(tag_stack: &[MarkdownTag]) -> bool {
    tag_stack.iter().any(|tag| {
        matches!(
            tag,
            MarkdownTag::CodeBlock { .. }
                | MarkdownTag::HtmlBlock
                | MarkdownTag::MetadataBlock(_)
                | MarkdownTag::Table(_)
                | MarkdownTag::TableHead
                | MarkdownTag::TableRow
                | MarkdownTag::TableCell
                | MarkdownTag::Image { .. }
        )
    })
}

/// Turns the buffered runs into whole sentences.
///
/// A flush is *final* when either the whole message is done, or this block's
/// content ends before `last_content_end` — meaning more already-parsed
/// content exists later in the document, so this block is pinned and will
/// never be revisited. A final flush speaks every sentence, including a
/// trailing fragment with no terminator (headings and short list items
/// routinely have none, and there is nothing left to wait for). A
/// non-final flush can only happen for the block sitting at the very end of
/// the document, which might still be mid-sentence on the next parse, so its
/// unterminated trailing fragment is withheld.
fn flush(
    runs: &mut Vec<TextRun>,
    utterances: &mut Vec<Utterance>,
    last_content_end: Option<usize>,
    message_complete: bool,
) {
    if runs.is_empty() {
        return;
    }

    let is_final = message_complete
        || runs.last().is_some_and(|run| {
            last_content_end.is_some_and(|last_content_end| run.source_range.end < last_content_end)
        });

    let mut combined = String::new();
    // Maps each byte index in `combined` back to a source byte index.
    let mut origins: Vec<usize> = Vec::new();
    for run in runs.iter() {
        for (offset, character) in run.text.char_indices() {
            // Skipped spans (inline code, hashes, URLs) leave the whitespace
            // on both sides behind; collapsing it here keeps the spoken text
            // natural ("in  and" → "in and") with the origin map in lockstep.
            if character.is_whitespace() && combined.ends_with(|c: char| c.is_whitespace()) {
                continue;
            }
            let source_index = run.source_range.start + offset.min(run.source_range.len());
            for _ in 0..character.len_utf8() {
                origins.push(source_index);
            }
            combined.push(character);
        }
    }
    runs.clear();

    let mut sentences = split_sentences(&combined);
    if is_final {
        let consumed_end = sentences.last().map_or(0, |sentence| sentence.end);
        if consumed_end < combined.len() {
            sentences.push(consumed_end..combined.len());
        }
    }

    for sentence in sentences {
        let text = combined[sentence.clone()].trim();
        // Covers the empty case too: a sentence with nothing alphanumeric —
        // everything speakable in it was skipped — is noise, not speech.
        if !text.chars().any(char::is_alphanumeric) {
            continue;
        }
        let leading =
            combined[sentence.clone()].len() - combined[sentence.clone()].trim_start().len();
        let start_index = sentence.start + leading;
        let end_index = start_index + text.len();
        let Some(&source_start) = origins.get(start_index) else {
            continue;
        };
        let source_end = origins
            .get(end_index.saturating_sub(1))
            .map_or(source_start, |last| last + 1);
        let Some(spoken_origins) = origins.get(start_index..end_index) else {
            continue;
        };
        utterances.push(Utterance {
            source_range: source_start..source_end,
            spoken_text: text.to_string(),
            spoken_origins: spoken_origins.to_vec(),
        });
    }
}

/// Splits on `.`/`?`/`!` followed by whitespace, guarding abbreviations,
/// decimals, ordered-list markers, and file extensions. Any trailing fragment
/// without a terminator is left off the end — `flush` decides whether that
/// fragment should still be spoken, based on finality rather than on this
/// function's output alone.
fn split_sentences(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut sentences = Vec::new();
    let mut start = 0;

    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(byte, b'.' | b'?' | b'!') {
            continue;
        }
        let after = index + 1;
        let followed_by_whitespace = bytes
            .get(after)
            .is_some_and(|next| next.is_ascii_whitespace());
        if !followed_by_whitespace && after != bytes.len() {
            continue;
        }
        if *byte == b'.' && !terminates_sentence(text, index) {
            continue;
        }
        sentences.push(start..after);
        start = after;
    }

    sentences
}

/// Decides whether the period at `index` really ends a sentence.
fn terminates_sentence(text: &str, index: usize) -> bool {
    let bytes = text.as_bytes();
    let before = &text[..index + 1];

    if ABBREVIATIONS
        .iter()
        .any(|abbreviation| before.ends_with(abbreviation))
    {
        return false;
    }

    let previous = index.checked_sub(1).and_then(|i| bytes.get(i));
    let next = bytes.get(index + 1);

    // Decimals: 3.50 — digit on both sides.
    if previous.is_some_and(u8::is_ascii_digit) && next.is_some_and(u8::is_ascii_digit) {
        return false;
    }

    // Ordered list markers: "1." at the start of a run.
    if previous.is_some_and(u8::is_ascii_digit)
        && text[..index]
            .rfind(|c: char| !c.is_ascii_digit())
            .is_none_or(|i| text[i..].starts_with(|c: char| c.is_whitespace()) || i + 1 == index)
        && text[..index]
            .trim_start()
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        return false;
    }

    // File extensions: word character on both sides with no space, e.g. main.rs
    if previous.is_some_and(|b| b.is_ascii_alphanumeric())
        && next.is_some_and(u8::is_ascii_alphabetic)
    {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use markdown::Markdown;

    fn utterances(source: &str, message_complete: bool, cx: &mut TestAppContext) -> Vec<Utterance> {
        // `Markdown::new_text` runs the link-only parser, which never emits the
        // block structure (RootEnd, Paragraph/Heading/Item, CodeBlock, Table...)
        // the segmenter depends on. Use the full parser instead.
        let markdown = cx.new(|cx| Markdown::new(source.into(), None, None, cx));
        cx.run_until_parked();
        markdown.read_with(cx, |markdown, _| {
            segment(markdown.parsed_markdown(), message_complete)
        })
    }

    fn spoken(source: &str, cx: &mut TestAppContext) -> Vec<String> {
        utterances(source, false, cx)
            .into_iter()
            .map(|utterance| utterance.spoken_text)
            .collect()
    }

    fn spoken_complete(source: &str, cx: &mut TestAppContext) -> Vec<String> {
        utterances(source, true, cx)
            .into_iter()
            .map(|utterance| utterance.spoken_text)
            .collect()
    }

    #[gpui::test]
    fn splits_a_paragraph_into_sentences(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("First one. Second one! Third one?\n", cx),
            vec!["First one.", "Second one!", "Third one?"]
        );
    }

    #[gpui::test]
    fn source_ranges_point_at_original_bytes(cx: &mut TestAppContext) {
        let source = "Alpha. Beta.\n";
        let utterances = utterances(source, false, cx);
        assert_eq!(&source[utterances[0].source_range.clone()], "Alpha.");
        assert_eq!(&source[utterances[1].source_range.clone()], "Beta.");
    }

    #[gpui::test]
    fn skips_code_blocks(cx: &mut TestAppContext) {
        let source = "Before it.\n\n```rust\nlet x = 1. Not prose.\n```\n\nAfter it.\n";
        assert_eq!(spoken(source, cx), vec!["Before it.", "After it."]);
    }

    #[gpui::test]
    fn skips_tables(cx: &mut TestAppContext) {
        let source = "Intro here.\n\n| a | b |\n|---|---|\n| 1. | 2. |\n\nOutro here.\n";
        assert_eq!(spoken(source, cx), vec!["Intro here.", "Outro here."]);
    }

    #[gpui::test]
    fn speaks_headings_and_list_items(cx: &mut TestAppContext) {
        let source = "# A heading\n\n- First bullet.\n- Second bullet.\n";
        assert_eq!(
            spoken(source, cx),
            vec!["A heading", "First bullet.", "Second bullet."]
        );
    }

    #[gpui::test]
    fn speaks_nested_bullets(cx: &mut TestAppContext) {
        let source = "- Outer one.\n  - Inner one.\n";
        assert_eq!(spoken(source, cx), vec!["Outer one.", "Inner one."]);
    }

    #[gpui::test]
    fn strips_inline_markup_from_spoken_text(cx: &mut TestAppContext) {
        let source = "Use **bold** and `code` and [a link](https://example.com) here.\n";
        assert_eq!(
            spoken(source, cx),
            vec!["Use bold and code and a link here."]
        );
    }

    #[gpui::test]
    fn spoken_ranges_map_back_to_source_through_markup(cx: &mut TestAppContext) {
        let source = "Use **bold** and `code` and [a link](https://example.com) here.\n";
        let utterances = utterances(source, false, cx);
        let utterance = &utterances[0];
        assert_eq!(utterance.spoken_text, "Use bold and code and a link here.");

        for word in ["Use", "bold", "code", "a link", "here."] {
            let spoken_start = utterance
                .spoken_text
                .find(word)
                .expect("word is in the spoken text");
            let source_range = utterance
                .source_range_for_spoken(spoken_start..spoken_start + word.len())
                .expect("word maps back to the source");
            assert_eq!(
                &source[source_range], word,
                "spoken '{word}' must map to the same text in the source"
            );
        }
    }

    #[gpui::test]
    fn spoken_ranges_map_back_to_source_after_a_muted_block(cx: &mut TestAppContext) {
        let source = "Before it.\n\n```rust\nlet x = 1;\n```\n\nAfter it.\n";
        let utterances = utterances(source, false, cx);
        let utterance = &utterances[1];
        assert_eq!(utterance.spoken_text, "After it.");

        let spoken_start = utterance.spoken_text.find("After").expect("word exists");
        let source_range = utterance
            .source_range_for_spoken(spoken_start..spoken_start + "After".len())
            .expect("word maps back to the source");
        assert_eq!(&source[source_range], "After");
    }

    #[gpui::test]
    fn spoken_ranges_map_back_to_source_across_a_soft_break(cx: &mut TestAppContext) {
        let source = "First part\nsecond part.\n";
        let utterances = utterances(source, false, cx);
        let utterance = &utterances[0];
        assert_eq!(utterance.spoken_text, "First part second part.");

        let spoken_start = utterance.spoken_text.find("second").expect("word exists");
        let source_range = utterance
            .source_range_for_spoken(spoken_start..spoken_start + "second".len())
            .expect("word maps back to the source");
        assert_eq!(&source[source_range], "second");
    }

    #[gpui::test]
    fn an_empty_or_out_of_bounds_spoken_range_maps_to_nothing(cx: &mut TestAppContext) {
        let utterances = utterances("Alpha.\n", false, cx);
        let utterance = &utterances[0];
        assert_eq!(utterance.source_range_for_spoken(2..2), None);
        assert_eq!(utterance.source_range_for_spoken(0..999), None);
    }

    #[gpui::test]
    fn does_not_split_on_abbreviations(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("Use e.g. this one. Or i.e. that one.\n", cx),
            vec!["Use e.g. this one.", "Or i.e. that one."]
        );
        assert_eq!(
            spoken("Ask Dr. Who about it.\n", cx),
            vec!["Ask Dr. Who about it."]
        );
    }

    #[gpui::test]
    fn does_not_split_on_decimals(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("It costs 3.50 total.\n", cx),
            vec!["It costs 3.50 total."]
        );
    }

    #[gpui::test]
    fn does_not_split_on_file_extensions(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("Edit main.rs and app.ts now.\n", cx),
            vec!["Edit main.rs and app.ts now."]
        );
    }

    #[gpui::test]
    fn does_not_split_on_ordered_list_markers(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("1. First step here.\n2. Second step here.\n", cx),
            vec!["First step here.", "Second step here."]
        );
    }

    #[gpui::test]
    fn ignores_an_unterminated_trailing_sentence(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("Complete one. Still typing", cx),
            vec!["Complete one."]
        );
    }

    #[gpui::test]
    fn ignores_an_unclosed_code_fence_while_streaming(cx: &mut TestAppContext) {
        let source = "Prose first.\n\n```rust\nlet total = 1. This is code.\n";
        assert_eq!(spoken(source, cx), vec!["Prose first."]);
    }

    #[gpui::test]
    fn produces_nothing_for_empty_input(cx: &mut TestAppContext) {
        assert_eq!(spoken("", cx), Vec::<String>::new());
    }

    #[gpui::test]
    fn withholds_a_half_typed_heading(cx: &mut TestAppContext) {
        // The heading tag closes (pulldown_cmark closes every tag at EOF),
        // but it is the last content in the document, so it might still grow
        // into a different heading on the next parse. It must stay silent
        // until either more content follows it or the message is complete.
        assert_eq!(spoken("# Half typed head", cx), Vec::<String>::new());
    }

    #[gpui::test]
    fn withholds_a_half_typed_bullet(cx: &mut TestAppContext) {
        assert_eq!(spoken("- First bul", cx), Vec::<String>::new());
    }

    #[gpui::test]
    fn speaks_a_finished_block_without_a_terminator_when_more_content_follows(
        cx: &mut TestAppContext,
    ) {
        // The colon never terminates a sentence, but "Here is the plan:" is
        // spoken anyway because content after it (the bullet) proves the
        // parser has moved past it for good — finality, not punctuation,
        // gates whether an unterminated fragment is spoken.
        let source = "Here is the plan:\n\n- Do it.\n";
        assert_eq!(spoken(source, cx), vec!["Here is the plan:", "Do it."]);
    }

    #[gpui::test]
    fn speaks_the_trailing_fragment_once_the_message_is_complete(cx: &mut TestAppContext) {
        assert_eq!(
            spoken_complete("Complete one. Still typing", cx),
            vec!["Complete one.", "Still typing"]
        );
    }

    #[gpui::test]
    fn speaks_word_like_inline_code_with_separators_as_spaces(cx: &mut TestAppContext) {
        assert_eq!(
            spoken(
                "Set `speaking_rate` and `read-aloud` and `enabled` now.\n",
                cx
            ),
            vec!["Set speaking rate and read aloud and enabled now."]
        );
    }

    #[gpui::test]
    fn skips_path_and_code_like_inline_code(cx: &mut TestAppContext) {
        assert_eq!(
            spoken(
                "The fix is in `crates/read_aloud/src/player.rs:250` and ready.\n",
                cx
            ),
            vec!["The fix is in and ready."]
        );
        assert_eq!(
            spoken("Run `cargo test -p read_aloud` locally.\n", cx),
            vec!["Run locally."]
        );
        assert_eq!(
            spoken("Check `main.rs` and `let x = 1` here.\n", cx),
            vec!["Check and here."]
        );
    }

    #[gpui::test]
    fn skips_hash_like_tokens_even_outside_backticks(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("Fixed in commit aa370aca7c today.\n", cx),
            vec!["Fixed in commit today."]
        );
        assert_eq!(spoken("See `aa370aca7c` too.\n", cx), vec!["See too."]);
        assert_eq!(
            spoken("It served 1234567 requests.\n", cx),
            vec!["It served 1234567 requests."],
            "plain numbers are not hashes"
        );
    }

    #[gpui::test]
    fn skips_bare_urls_but_keeps_link_text(cx: &mut TestAppContext) {
        assert_eq!(
            spoken("See https://example.com/docs for more.\n", cx),
            vec!["See for more."]
        );
        assert_eq!(
            spoken("See <https://example.com> for more.\n", cx),
            vec!["See for more."]
        );
        assert_eq!(
            spoken("See [the docs](https://example.com) for more.\n", cx),
            vec!["See the docs for more."]
        );
    }

    #[gpui::test]
    fn a_skipped_span_keeps_sentence_flow_and_origins(cx: &mut TestAppContext) {
        let source = "The fix is in `player.rs:250` and ready.\n";
        let utterances = utterances(source, false, cx);
        assert_eq!(utterances.len(), 1);
        let utterance = &utterances[0];
        assert_eq!(utterance.spoken_text, "The fix is in and ready.");

        // The sentence wash still covers the skipped span...
        let code_start = source.find("`player").expect("code span exists");
        let code_end = source.find(" and").expect("code span ends");
        assert!(utterance.source_range.start < code_start);
        assert!(utterance.source_range.end > code_end);

        // ...but no spoken byte maps into it, so the word pill can never
        // land there.
        let code_range = code_start..code_end;
        assert!(
            utterance
                .spoken_origins
                .iter()
                .all(|origin| !code_range.contains(origin))
        );

        // Words after the skip still map to the right source bytes.
        let spoken_start = utterance
            .spoken_text
            .find("and")
            .expect("word survives the skip");
        let mapped = utterance
            .source_range_for_spoken(spoken_start..spoken_start + "and".len())
            .expect("word maps back to the source");
        assert_eq!(&source[mapped], "and");
    }
}
