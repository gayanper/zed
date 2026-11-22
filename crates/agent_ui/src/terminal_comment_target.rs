use std::ops::RangeInclusive;

use terminal::{Content as TerminalContent, GridRow, Range};
use terminal_view::is_box_drawing;

/// Footer hints Claude Code shows under an interactive picker, such as its
/// question and permission prompts. Comments go into the prompt, which the
/// picker replaces until it's answered.
const PICKER_HINTS: &[&str] = &["Enter to select", "Esc to cancel"];
/// How many of the last rows with text are searched for picker hints.
const PICKER_HINT_ROWS: usize = 12;

/// Decorations that TUIs draw at the start of a line, which aren't part of
/// the text and are left out of quotes. A `>` stays, since it's also how
/// Markdown quotes text.
const DECORATIONS: &[char] = &['⏺', '●', '⎿', '❯'];

/// The columns of a row's text, without surrounding whitespace and box
/// borders.
fn text_columns(characters: &[char]) -> Option<RangeInclusive<usize>> {
    let is_text = |character: &char| !character.is_whitespace() && !is_box_drawing(*character);
    let start = characters.iter().position(is_text)?;
    let end = characters.iter().rposition(is_text)?;
    Some(start..=end)
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

/// The rows of `range`'s lines to quote, with the columns of their text: no
/// decorations, box borders or surrounding whitespace, the first line cut at
/// the range's start and the last line cut at the range's end. A row the
/// terminal wrapped keeps its trailing spaces, which separate it from the
/// next row's text. Each row says whether it wraps.
pub(crate) fn quote_rows(
    grid_rows: &[GridRow],
    range: &Range,
) -> Vec<(i32, RangeInclusive<usize>, bool)> {
    let start = range.start();
    let end = range.end();
    grid_rows
        .iter()
        .filter(|row| (range.start().line..=end.line).contains(&row.line))
        .filter_map(|row| {
            let last_column = row.characters.len().checked_sub(1)?;
            let columns = match text_columns(&row.characters) {
                Some(columns) if row.wraps => *columns.start()..=last_column,
                Some(columns) => columns,
                None if row.wraps => 0..=last_column,
                None => return None,
            };
            let columns = without_decoration(&row.characters, columns);
            let columns = if row.line == start.line {
                (*columns.start()).max(start.column)..=*columns.end()
            } else {
                columns
            };
            let columns = if row.line == end.line {
                *columns.start()..=(*columns.end()).min(end.column)
            } else {
                columns
            };
            (columns.start() <= columns.end()).then_some((row.line, columns, row.wraps))
        })
        .collect()
}

/// Whether `range` leaves out text before its start on its line, and after
/// its end on its line. Decorations and box borders aren't text, and a row
/// the terminal wrapped onto another continues its text there.
pub(crate) fn trimmed_edges(grid_rows: &[GridRow], range: &Range) -> (bool, bool) {
    let row_at = |line: i32| grid_rows.iter().find(|row| row.line == line);
    let text_at = |row: &GridRow| {
        text_columns(&row.characters).map(|columns| without_decoration(&row.characters, columns))
    };
    let start = range.start();
    let end = range.end();
    let start_trimmed = start
        .line
        .checked_sub(1)
        .and_then(row_at)
        .is_some_and(|row| row.wraps)
        || row_at(start.line)
            .and_then(text_at)
            .is_some_and(|columns| *columns.start() < start.column);
    let end_trimmed = row_at(end.line).is_some_and(|row| {
        row.wraps || text_at(row).is_some_and(|columns| *columns.end() > end.column)
    });
    (start_trimmed, end_trimmed)
}

/// Joins the text of quoted rows: rows the terminal wrapped continue the
/// line.
pub(crate) fn quote_text(
    rows: &[(i32, RangeInclusive<usize>, bool)],
    rows_text: &[String],
) -> String {
    let mut text = String::new();
    for ((_, _, wraps), row_text) in rows.iter().zip(rows_text) {
        text.push_str(row_text);
        if !wraps {
            text.push('\n');
        }
    }
    text.trim_end().to_string()
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
    use terminal::Point;

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

    /// The quote of `range` in `rows`, with each row's text taken from its
    /// characters.
    fn quote(rows: &[GridRow], range: Range) -> String {
        let quoted = quote_rows(rows, &range);
        let texts = quoted
            .iter()
            .map(|(line, columns, _)| {
                rows.iter()
                    .find(|row| row.line == *line)
                    .map(|row| row.characters[columns.clone()].iter().collect::<String>())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        quote_text(&quoted, &texts)
    }

    fn lines(first: i32, last: i32) -> Range {
        Range::new(Point::new(first, 0), Point::new(last, usize::MAX))
    }

    #[test]
    fn test_a_partial_range_is_quoted_and_marked_trimmed() {
        let rows = grid("alpha beta gamma\ndelta epsilon\n");
        let range = Range::new(Point::new(0, 6), Point::new(1, 4));
        assert_eq!(quote(&rows, range), "beta gamma\ndelta");
        assert_eq!(trimmed_edges(&rows, &range), (true, true));
        assert_eq!(trimmed_edges(&rows, &lines(0, 1)), (false, false));
        let range = Range::new(Point::new(0, 0), Point::new(0, 15));
        assert_eq!(trimmed_edges(&rows, &range), (false, false));

        let rows = grid("⏺ done here\nfull row\\\n foo bar\n");
        let range = Range::new(Point::new(0, 2), Point::new(0, 5));
        assert_eq!(
            trimmed_edges(&rows, &range),
            (false, true),
            "a decoration isn't text"
        );
        let range = Range::new(Point::new(2, 1), Point::new(2, 7));
        assert_eq!(
            trimmed_edges(&rows, &range),
            (true, false),
            "a wrapped row continues the text before it"
        );
    }

    #[test]
    fn test_decorations_are_left_out() {
        let rows = grid("⏺ The build failed.\n  ⎿ Fix the path.\n❯ retry\n> quoted\n");
        assert_eq!(
            quote(&rows, lines(0, 3)),
            "The build failed.\nFix the path.\nretry\n> quoted"
        );
    }

    #[test]
    fn test_box_borders_are_left_out() {
        let rows = grid(indoc::indoc! {"
            ╭──────────────────╮
            │ - boxed bullet   │
            │   continued      │
            ╰──────────────────╯
        "});
        assert_eq!(quote(&rows, lines(0, 3)), "- boxed bullet\ncontinued");
    }

    #[test]
    fn test_wrapped_rows_are_one_line() {
        let rows = grid("plain text that the \\\nterminal wrapped\n");
        assert_eq!(
            quote(&rows, lines(0, 1)),
            "plain text that the terminal wrapped"
        );
    }

    #[test]
    fn test_the_last_line_ends_at_the_range_end() {
        let rows = grid("alpha beta\ngamma delta\n");
        assert_eq!(
            quote(&rows, Range::new(Point::new(0, 0), Point::new(1, 4))),
            "alpha beta\ngamma"
        );
    }
}
