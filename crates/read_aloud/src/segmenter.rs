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
            MarkdownEvent::Text | MarkdownEvent::Code => {
                if !is_muted(&tag_stack) {
                    if let Some(text) = source.get(range.clone()) {
                        runs.push(TextRun {
                            source_range: range.clone(),
                            text: text.to_string(),
                        });
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
            let source_index = run.source_range.start + offset.min(run.source_range.len());
            for _ in 0..character.len_utf8() {
                origins.push(source_index);
            }
        }
        combined.push_str(&run.text);
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
        if text.is_empty() {
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
        utterances.push(Utterance {
            source_range: source_start..source_end,
            spoken_text: text.to_string(),
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
}
