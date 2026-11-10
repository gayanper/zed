use std::ops::RangeInclusive;

use agent_settings::TerminalCommentPattern;
use terminal::{Content as TerminalContent, GridRow};

/// The most rows a comment target spans, and how far from the pointed row
/// its start and end are looked for, so a long run of indented output can't
/// quote a whole log.
pub(crate) const MAX_TARGET_ROWS: usize = 200;

/// Footer hints Claude Code shows under an interactive picker, such as its
/// question and permission prompts. Comments go into the prompt, which the
/// picker replaces until it's answered.
const PICKER_HINTS: &[&str] = &["Enter to select", "Esc to cancel"];
/// How many of the last rows with text are searched for picker hints.
const PICKER_HINT_ROWS: usize = 12;

/// Box borders aren't part of the text they surround, so targets, quotes
/// and comment anchors leave them out.
pub(crate) fn is_box_drawing(character: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&character)
}

/// Characters that comment anchors skip: whitespace and box borders change
/// when a TUI redraws or the terminal resizes, and an ellipsis marks a
/// truncated quote.
pub(crate) fn is_ignored_by_anchor(character: char) -> bool {
    character.is_whitespace() || is_box_drawing(character) || character == '…'
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TargetRow {
    pub line: i32,
    pub columns: RangeInclusive<usize>,
    /// The row's text continues on the next row, which the terminal wrapped.
    pub wraps: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommentTarget {
    pub rows: Vec<TargetRow>,
    /// Runs of grid lines that are one paragraph each.
    pub paragraphs: Vec<RangeInclusive<i32>>,
    /// The item continues past `rows`.
    pub truncated: bool,
}

struct LogicalLine {
    /// Indices into the rows, of the rows the terminal wrapped this line over.
    rows: RangeInclusive<usize>,
    indent: Option<usize>,
}

impl LogicalLine {
    fn is_blank(&self) -> bool {
        self.indent.is_none()
    }
}

/// The columns of a row's text, without surrounding whitespace and box
/// borders.
fn text_columns(characters: &[char]) -> Option<RangeInclusive<usize>> {
    let is_text = |character: &char| !character.is_whitespace() && !is_box_drawing(*character);
    let start = characters.iter().position(is_text)?;
    let end = characters.iter().rposition(is_text)?;
    Some(start..=end)
}

fn logical_lines(rows: &[GridRow]) -> Vec<LogicalLine> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        let mut end = start;
        while end + 1 < rows.len() && rows[end].wraps {
            end += 1;
        }
        let indent =
            (start..=end).find_map(|index| Some(*text_columns(&rows[index].characters)?.start()));
        lines.push(LogicalLine {
            rows: start..=end,
            indent,
        });
        start = end + 1;
    }
    lines
}

/// The index of the last line of the item whose marker is on line
/// `marker`: the lines after it that are indented deeper, up to a blank line.
fn item_end(lines: &[LogicalLine], marker: usize) -> usize {
    let Some(marker_indent) = lines[marker].indent else {
        return marker;
    };
    let mut end = marker;
    while let Some(next) = lines.get(end + 1)
        && next.indent.is_some_and(|indent| indent > marker_indent)
    {
        end += 1;
    }
    end
}

/// Markers of a new agent message or a user prompt, which end a pattern's
/// block even when it starts at the same indentation.
const MESSAGE_MARKERS: &[&str] = &["⏺ ", "❯ ", "> "];

/// The text that patterns match: the line's rows joined, without the cells
/// that pad wide characters and without surrounding whitespace, nor the
/// box borders around other text.
fn match_text(rows: &[GridRow], line: &LogicalLine) -> String {
    let mut text = String::new();
    for row in &rows[line.rows.clone()] {
        text.extend(
            row.characters
                .iter()
                .enumerate()
                .filter(|(column, _)| !row.wide_char_spacers.contains(column))
                .map(|(_, character)| *character),
        );
    }
    let text = text.trim();
    let without_borders =
        text.trim_matches(|character: char| character.is_whitespace() || is_box_drawing(character));
    // A line of only borders is a separator, which `end` patterns match.
    if without_borders.is_empty() {
        text.to_string()
    } else {
        without_borders.to_string()
    }
}

/// The match text of each line, and whether it starts a block, worked out
/// when first needed.
struct MatchTexts<'a> {
    rows: &'a [GridRow],
    lines: &'a [LogicalLine],
    patterns: &'a [TerminalCommentPattern],
    texts: Vec<Option<String>>,
    starts_block: Vec<Option<bool>>,
}

impl<'a> MatchTexts<'a> {
    fn new(
        rows: &'a [GridRow],
        lines: &'a [LogicalLine],
        patterns: &'a [TerminalCommentPattern],
    ) -> Self {
        Self {
            rows,
            lines,
            patterns,
            texts: vec![None; lines.len()],
            starts_block: vec![None; lines.len()],
        }
    }

    fn get(&mut self, index: usize) -> &str {
        let (rows, lines) = (self.rows, self.lines);
        self.texts[index].get_or_insert_with(|| match_text(rows, &lines[index]))
    }

    /// Whether the line matches the start of any pattern.
    fn starts_block(&mut self, index: usize) -> bool {
        if let Some(starts_block) = self.starts_block[index] {
            return starts_block;
        }
        let patterns = self.patterns;
        let text = self.get(index);
        let starts_block = patterns.iter().any(|pattern| pattern.start.is_match(text));
        self.starts_block[index] = Some(starts_block);
        starts_block
    }
}

/// The lines of the block of `pattern` that contains line `pointed`: from
/// the nearest line above that matches its start, to before the next line
/// that starts any pattern's block, matches its end, is indented less, or
/// starts a message at the same indentation.
fn pattern_block(
    lines: &[LogicalLine],
    texts: &mut MatchTexts,
    pointed: usize,
    pattern: &TerminalCommentPattern,
) -> Option<RangeInclusive<usize>> {
    let start = (0..=pointed)
        .rev()
        .find(|index| !lines[*index].is_blank() && pattern.start.is_match(texts.get(*index)))?;
    let start_indent = lines[start].indent?;
    let mut last = start;
    for index in start + 1..lines.len() {
        let text = texts.get(index);
        if !text.is_empty() && pattern.end.as_ref().is_some_and(|end| end.is_match(text)) {
            break;
        }
        let Some(indent) = lines[index].indent else {
            continue;
        };
        let starts_message = MESSAGE_MARKERS
            .iter()
            .any(|marker| text.starts_with(marker));
        if indent < start_indent
            || (indent == start_indent && starts_message)
            || texts.starts_block(index)
        {
            break;
        }
        last = index;
    }
    (pointed <= last).then_some(start..=last)
}

/// Markers that start an agent message or a user prompt: a message is the
/// marker's line and the lines indented deeper below it.
const MESSAGE_START_MARKERS: &[&str] = &["⏺ ", "● ", "❯ ", "> "];

/// Decorations that TUIs draw at the start of a line, which aren't part of
/// the text and are left out of targets. A `>` stays, since it's also how
/// Markdown quotes text.
const DECORATIONS: &[char] = &['⏺', '●', '⎿', '❯'];

fn starts_message(rows: &[GridRow], lines: &[LogicalLine], index: usize) -> bool {
    let text = match_text(rows, &lines[index]);
    MESSAGE_START_MARKERS
        .iter()
        .any(|marker| text.starts_with(marker))
}

/// The lines of the message that contains line `pointed`: from the nearest
/// line above that starts a message and is indented less than the lines
/// after it, to before the next line indented as little.
fn marked_message(
    rows: &[GridRow],
    lines: &[LogicalLine],
    pointed: usize,
) -> Option<RangeInclusive<usize>> {
    // The outermost start wins, so a quote or a bullet nested in a message
    // doesn't start a message of its own.
    let mut least_indent_below = usize::MAX;
    let mut start = None;
    for index in (0..=pointed).rev() {
        let Some(indent) = lines[index].indent else {
            continue;
        };
        if starts_message(rows, lines, index) && (index == pointed || indent < least_indent_below) {
            start = Some((index, indent));
        }
        if indent == 0 {
            break;
        }
        least_indent_below = least_indent_below.min(indent);
    }
    let (start, start_indent) = start?;
    let mut last = start;
    for index in start + 1..lines.len() {
        match lines[index].indent {
            Some(indent) if indent <= start_indent => break,
            Some(_) => last = index,
            None => {}
        }
    }
    Some(start..=last)
}

/// The run of lines with text that contains line `pointed`, without a
/// message start in it, ending where the lines are indented less than its
/// first.
fn unmarked_block(
    rows: &[GridRow],
    lines: &[LogicalLine],
    pointed: usize,
) -> RangeInclusive<usize> {
    let mut start = pointed;
    while let Some(previous) = start.checked_sub(1)
        && let (Some(previous_indent), Some(indent)) = (lines[previous].indent, lines[start].indent)
        && previous_indent <= indent
        && !starts_message(rows, lines, previous)
    {
        start = previous;
    }
    let start_indent = lines[start].indent.unwrap_or(0);
    let mut last = start;
    while let Some(next) = lines.get(last + 1)
        && next.indent.is_some_and(|indent| indent >= start_indent)
        && !starts_message(rows, lines, last + 1)
    {
        last += 1;
    }
    start..=last
}

/// The runs of lines in `message` that are one paragraph each: the blocks
/// of `patterns`, and questions with the lines indented deeper below them,
/// such as their options.
fn paragraph_runs(
    rows: &[GridRow],
    lines: &[LogicalLine],
    texts: &mut MatchTexts,
    message: &RangeInclusive<usize>,
    patterns: &[TerminalCommentPattern],
) -> Vec<RangeInclusive<usize>> {
    let mut runs = Vec::new();
    let mut index = *message.start();
    while index <= *message.end() {
        let run = if lines[index].is_blank() {
            None
        } else if let Some(block) = patterns
            .iter()
            .find_map(|pattern| pattern_block(lines, texts, index, pattern))
            .filter(|block| *block.start() == index)
        {
            Some(*block.start()..=(*block.end()).min(*message.end()))
        } else if !starts_message(rows, lines, index)
            && match_text(rows, &lines[index]).ends_with('?')
        {
            Some(index..=item_end(lines, index).min(*message.end()))
        } else {
            None
        };
        match run {
            Some(run) => {
                index = run.end() + 1;
                if run.start() != run.end() {
                    runs.push(run);
                }
            }
            None => index += 1,
        }
    }
    runs
}

/// The columns of a row's text, after a decoration it starts with.
fn without_decoration(
    characters: &[char],
    columns: RangeInclusive<usize>,
) -> RangeInclusive<usize> {
    let start = *columns.start();
    if !characters
        .get(start)
        .is_some_and(|character| DECORATIONS.contains(character))
    {
        return columns;
    }
    let text_start = (start + 1..=*columns.end()).find(|column| {
        characters
            .get(*column)
            .is_some_and(|character| !character.is_whitespace())
    });
    match text_start {
        Some(text_start) => text_start..=*columns.end(),
        None => columns,
    }
}

/// The message at `line`: an agent message or a user prompt with the lines
/// below it, else the run of lines with text around it. Inside a message,
/// the blocks of `patterns` and questions with their options are one
/// paragraph each. `rows` are consecutive grid rows around `line`, and
/// `more_below` says whether the grid continues after them.
pub(crate) fn find_comment_target(
    rows: &[GridRow],
    line: i32,
    more_below: bool,
    patterns: &[TerminalCommentPattern],
) -> Option<CommentTarget> {
    let lines = logical_lines(rows);
    let row_index = rows.iter().position(|row| row.line == line)?;
    let pointed = lines
        .iter()
        .position(|logical_line| logical_line.rows.contains(&row_index))?;
    if lines[pointed].is_blank() {
        return None;
    }

    let line_range = marked_message(rows, &lines, pointed)
        .unwrap_or_else(|| unmarked_block(rows, &lines, pointed));
    let mut texts = MatchTexts::new(rows, &lines, patterns);
    let paragraphs = paragraph_runs(rows, &lines, &mut texts, &line_range, patterns)
        .into_iter()
        .map(|run| {
            rows[*lines[*run.start()].rows.start()].line..=rows[*lines[*run.end()].rows.end()].line
        })
        .collect();
    let first_row = *lines[*line_range.start()].rows.start();
    let last_row = *lines[*line_range.end()].rows.end();
    let has_text = |index: &usize| text_columns(&rows[*index].characters).is_some();
    let first_text_row = (first_row..=last_row).find(has_text)?;
    let last_text_row = (first_row..=last_row).rev().find(has_text)?;
    let mut target_rows = (first_text_row..=last_text_row)
        .filter_map(|index| {
            let row = &rows[index];
            // A wrapped row's trailing spaces separate it from the next row's
            // text, even when the row has nothing else.
            let columns = match text_columns(&row.characters) {
                Some(columns) if row.wraps => {
                    *columns.start()..=row.characters.len().checked_sub(1)?
                }
                Some(columns) => columns,
                None if row.wraps => 0..=row.characters.len().checked_sub(1)?,
                None => return None,
            };
            Some(TargetRow {
                line: row.line,
                columns: without_decoration(&row.characters, columns),
                wraps: row.wraps,
            })
        })
        .collect::<Vec<_>>();
    let mut truncated = more_below && last_row + 1 == rows.len();
    if target_rows.len() > MAX_TARGET_ROWS {
        target_rows.truncate(MAX_TARGET_ROWS);
        truncated = true;
    }
    Some(CommentTarget {
        rows: target_rows,
        paragraphs,
        truncated,
    })
}

/// Joins the text of a target's rows: rows the terminal wrapped continue
/// the line, and an ellipsis marks text cut off at the end.
pub(crate) fn target_text(target: &CommentTarget, rows_text: &[String]) -> String {
    let mut text = String::new();
    for (row, row_text) in target.rows.iter().zip(rows_text) {
        text.push_str(row_text);
        if !row.wraps {
            text.push('\n');
        }
    }
    let mut text = text.trim_end().to_string();
    if target.truncated {
        text.push('…');
    }
    text
}

/// Whether an interactive picker is open: its hints are in the last rows
/// with text on screen, which aren't the screen's last rows until the
/// output fills it.
pub(crate) fn picker_visible(content: &TerminalContent) -> bool {
    let mut rows = Vec::<String>::new();
    let mut current_line = None;
    for cell in &content.cells {
        if current_line != Some(cell.point.line) {
            current_line = Some(cell.point.line);
            rows.push(String::new());
        }
        if let Some(row) = rows.last_mut() {
            row.push(cell.character());
        }
    }
    rows.iter()
        .rev()
        .filter(|row| !row.trim().is_empty())
        .take(PICKER_HINT_ROWS)
        .any(|row| PICKER_HINTS.iter().any(|hint| row.contains(hint)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_view::starts_with_list_marker;

    fn grid(text: &str) -> Vec<GridRow> {
        text.lines()
            .enumerate()
            .map(|(index, line)| {
                let (line_text, wraps) = match line.strip_suffix('\\') {
                    Some(line_text) => (line_text, true),
                    None => (line, false),
                };
                GridRow {
                    line: index as i32,
                    characters: line_text.chars().collect(),
                    wide_char_spacers: Vec::new(),
                    wraps,
                }
            })
            .collect()
    }

    fn target_lines(rows: &[GridRow], line: i32) -> Option<RangeInclusive<i32>> {
        pattern_target_lines(rows, line, &[])
    }

    fn pattern_target_lines(
        rows: &[GridRow],
        line: i32,
        patterns: &[TerminalCommentPattern],
    ) -> Option<RangeInclusive<i32>> {
        let target = find_comment_target(rows, line, false, patterns)?;
        Some(target.rows.first()?.line..=target.rows.last()?.line)
    }

    fn target_paragraphs(
        rows: &[GridRow],
        line: i32,
        patterns: &[TerminalCommentPattern],
    ) -> Vec<RangeInclusive<i32>> {
        find_comment_target(rows, line, false, patterns)
            .map(|target| target.paragraphs)
            .unwrap_or_default()
    }

    #[test]
    fn test_agent_message_with_paragraphs_is_one_target() {
        let rows = grid(indoc::indoc! {"
            ⏺ The build failed.

              I fixed the linker flags:
              - first step
                continued
              - second step
            > my answer
            ⏺ Done.
        "});
        for line in [0, 2, 3, 4, 5] {
            assert_eq!(target_lines(&rows, line), Some(0..=5), "line {line}");
        }
        assert_eq!(target_lines(&rows, 1), None);
        assert_eq!(target_lines(&rows, 6), Some(6..=6));
        assert_eq!(target_lines(&rows, 7), Some(7..=7));
    }

    #[test]
    fn test_nested_quotes_and_bullets_stay_in_their_message() {
        let rows = grid(indoc::indoc! {"
            ⏺ You wrote:
              > quoted text
                more quote
              ● a bullet
        "});
        for line in 0..=3 {
            assert_eq!(target_lines(&rows, line), Some(0..=3), "line {line}");
        }
    }

    #[test]
    fn test_message_markers_are_left_out() {
        let rows = grid("⏺ Read the file\n  ⎿  Read 5 lines\n");
        let target = find_comment_target(&rows, 1, false, &[]).unwrap();
        assert_eq!(
            target
                .rows
                .iter()
                .map(|row| row.columns.clone())
                .collect::<Vec<_>>(),
            vec![2..=14, 5..=16]
        );
    }

    #[test]
    fn test_unmarked_text_blocks() {
        let rows = grid(indoc::indoc! {"
            Plain output line
            another plain line
              indented detail

            Second block
            back at the start
            $ cargo test
        "});
        assert_eq!(target_lines(&rows, 0), Some(0..=2));
        assert_eq!(target_lines(&rows, 2), Some(0..=2));
        assert_eq!(target_lines(&rows, 3), None);
        assert_eq!(target_lines(&rows, 5), Some(4..=6));
    }

    #[test]
    fn test_indent_drop_ends_an_unmarked_block() {
        let rows = grid("    indented\nback at the start\n");
        assert_eq!(target_lines(&rows, 0), Some(0..=0));
        assert_eq!(target_lines(&rows, 1), Some(1..=1));
    }

    #[test]
    fn test_question_with_options_is_one_paragraph() {
        let rows = grid(indoc::indoc! {"
            ⏺ Some intro text.

              1. Which auth method should we use?
                 a) OAuth
                 b) API key
                    with a second line
              2. Should tokens expire?
              3. Plain item
        "});
        assert_eq!(target_lines(&rows, 4), Some(0..=7));
        assert_eq!(target_paragraphs(&rows, 4, &[]), vec![2..=5]);
    }

    #[test]
    fn test_box_borders_are_left_out() {
        let rows = grid(indoc::indoc! {"
            ╭──────────────────╮
            │ - boxed bullet   │
            │   continued      │
            ╰──────────────────╯
        "});
        let target = find_comment_target(&rows, 2, false, &[]).unwrap();
        assert_eq!(
            target.rows,
            vec![
                TargetRow {
                    line: 1,
                    columns: 2..=15,
                    wraps: false,
                },
                TargetRow {
                    line: 2,
                    columns: 4..=12,
                    wraps: false,
                },
            ]
        );
        assert_eq!(find_comment_target(&rows, 0, false, &[]), None);
    }

    #[test]
    fn test_wrapped_rows_are_one_line() {
        let rows = grid("plain text that the \\\nterminal wrapped\n");
        let target = find_comment_target(&rows, 1, false, &[]).unwrap();
        assert_eq!(target.rows.len(), 2);
        assert_eq!(target.rows[0].columns, 0..=19);
        assert_eq!(
            target_text(
                &target,
                &[
                    "plain text that the ".to_string(),
                    "terminal wrapped".to_string()
                ]
            ),
            "plain text that the terminal wrapped"
        );
    }

    #[test]
    fn test_cjk_text_with_wide_characters() {
        let mut rows = grid("⏺ 界 wide\n  更多 text\n");
        rows[0].characters.insert(3, ' ');
        rows[0].wide_char_spacers = vec![3];
        assert_eq!(target_lines(&rows, 1), Some(0..=1));
    }

    #[test]
    fn test_markers() {
        for text in ["- a", "* a", "• a", "❯ a", "1. a", "12) a", "a) a", "B. a"] {
            assert!(
                starts_with_list_marker(&text.chars().collect::<Vec<_>>()),
                "{text}"
            );
        }
        for text in ["-a", "e.g. a", "1.5 a", "1234. a", "ab) a", "text"] {
            assert!(
                !starts_with_list_marker(&text.chars().collect::<Vec<_>>()),
                "{text}"
            );
        }
    }

    #[test]
    fn test_long_messages_are_truncated() {
        let mut text = String::from("⏺ long message\n");
        for _ in 0..MAX_TARGET_ROWS + 10 {
            text.push_str("  more\n");
        }
        let rows = grid(&text);
        let target = find_comment_target(&rows, 0, false, &[]).unwrap();
        assert_eq!(target.rows.len(), MAX_TARGET_ROWS);
        assert!(target.truncated);
        assert!(target_text(&target, &["long message".to_string()]).ends_with('…'));
    }

    fn default_patterns() -> Vec<TerminalCommentPattern> {
        let compile = |pattern: &str| {
            agent_settings::CompiledRegex::try_new(pattern, true).expect("valid test regex")
        };
        [
            r"^(⏺\s+)?(❓\s*)?(\*\*)?Q\d+(\*\*)?\s*[-–—:.)]",
            r"^(⏺\s+)?(\*\*)?Question\s+\d+\s*[-–—:.)]",
        ]
        .into_iter()
        .map(|start| TerminalCommentPattern {
            start: compile(start),
            end: Some(compile("^[─━-]{3,}$")),
        })
        .collect()
    }

    #[test]
    fn test_grilling_questions_are_paragraphs() {
        let rows = grid(indoc::indoc! {"
            ⏺ Some questions:

              ❓ Q1 - Auth method:
              Which auth method should we use?

              ➡ Recommended: OAuth
                - because it's standard
              ────────────
              ❓ Q2 - Expiry:
              ➡ Recommended: one hour
            > my answer
        "});
        let patterns = default_patterns();
        assert_eq!(pattern_target_lines(&rows, 5, &patterns), Some(0..=9));
        assert_eq!(target_paragraphs(&rows, 5, &patterns), vec![2..=6, 8..=9]);
        assert_eq!(pattern_target_lines(&rows, 10, &patterns), Some(10..=10));
    }

    #[test]
    fn test_pattern_at_column_zero_starts_a_message() {
        let rows = grid(indoc::indoc! {"
            ⏺ Q2 - Expiry
              ➡ one hour

            ⏺ Next message
              more text
            > my answer
            Q3 revenue grew
        "});
        let patterns = default_patterns();
        assert_eq!(pattern_target_lines(&rows, 1, &patterns), Some(0..=1));
        assert_eq!(target_paragraphs(&rows, 1, &patterns), vec![0..=1]);
        assert_eq!(pattern_target_lines(&rows, 4, &patterns), Some(3..=4));
        assert_eq!(pattern_target_lines(&rows, 6, &patterns), Some(6..=6));
    }

    #[test]
    fn test_first_pattern_wins() {
        let compile = |pattern: &str| {
            agent_settings::CompiledRegex::try_new(pattern, true).expect("valid test regex")
        };
        let rows = grid("Q1\nfirst\n---\nsecond\n");
        let patterns = vec![
            TerminalCommentPattern {
                start: compile(r"^Q\d"),
                end: Some(compile("^---$")),
            },
            TerminalCommentPattern {
                start: compile(r"^Q\d"),
                end: None,
            },
        ];
        assert_eq!(target_paragraphs(&rows, 1, &patterns), vec![0..=1]);
        let reversed = patterns.into_iter().rev().collect::<Vec<_>>();
        assert_eq!(target_paragraphs(&rows, 1, &reversed), vec![0..=3]);
    }

    #[test]
    fn test_wide_char_spacers_are_left_out_of_match_text() {
        let mut rows = grid("❓  Q1 - Title\n");
        rows[0].wide_char_spacers = vec![1];
        let lines = logical_lines(&rows);
        assert_eq!(match_text(&rows, &lines[0]), "❓ Q1 - Title");
    }

    #[test]
    fn test_box_borders_are_left_out_of_match_text() {
        let rows = grid("│ Q1 - Title │\n│ ──────── │\n");
        let lines = logical_lines(&rows);
        assert_eq!(match_text(&rows, &lines[0]), "Q1 - Title");
        assert_eq!(match_text(&rows, &lines[1]), "│ ──────── │");
    }
}
