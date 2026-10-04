//! Drawing the picker inside its inline box: the list (projects, palette
//! actions, health issues, or the key help), an optional detail pane, the
//! input line, and a hint / status / confirmation line.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::matcher;
use crate::output;

use super::actions::HELP;
use super::picker::{Mode, Picker, sort_label};

/// Rows the detail pane takes from the list when it is open.
const DETAIL_ROWS: u16 = 4;

/// Draws one frame into the box ratatui sized for us; `picker` records where
/// the list landed so mouse clicks can be mapped back to rows.
pub fn draw(
    frame: &mut Frame,
    picker: &mut Picker,
    home: Option<&Path>,
    color: bool,
    border: bool,
) {
    let palette = Palette { color };
    let area = frame.area();
    let inner = if border {
        let block = Block::bordered().border_style(palette.dim());
        frame.render_widget(block.clone(), area);
        block.inner(area)
    } else {
        area
    };

    // The detail pane only makes sense over the project list.
    let details = picker.details_visible();
    // The box keeps its height: the pane takes rows from the list.
    let detail_rows = if details {
        DETAIL_ROWS.min(inner.height.saturating_sub(3))
    } else {
        0
    };

    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(detail_rows),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    picker.set_viewport(rows[0].height as usize);
    draw_view(frame, rows[0], picker, home, &palette);
    if detail_rows > 0 {
        draw_details(frame, rows[1], picker, home, &palette);
    }
    draw_input(frame, rows[2], picker, &palette);
    draw_status(frame, rows[3], picker, &palette);
}

/// The list area, whichever view is active.
fn draw_view(
    frame: &mut Frame,
    area: Rect,
    picker: &mut Picker,
    home: Option<&Path>,
    palette: &Palette,
) {
    match picker.mode() {
        Mode::Palette => draw_palette(frame, area, picker, palette),
        Mode::Help => draw_help(frame, area, picker, palette),
        Mode::Health => draw_health(frame, area, picker, palette),
        Mode::Search | Mode::Nav | Mode::Prompt => {
            draw_projects(frame, area, picker, home, palette)
        }
    }
}

fn draw_projects(
    frame: &mut Frame,
    area: Rect,
    picker: &mut Picker,
    home: Option<&Path>,
    palette: &Palette,
) {
    let count = picker.total_len();
    let selected = picker.selected();

    if count == 0 {
        let text = if picker.query().trim().is_empty() {
            if picker.projects().is_empty() {
                if picker.home_enabled() {
                    "no projects indexed — type to search your home directory"
                } else {
                    "no projects indexed"
                }
            } else if !picker.show_missing() {
                "every project is missing (hidden)"
            } else {
                "no projects indexed"
            }
        } else {
            "no matches"
        };
        picker.set_list_geometry(area, 0);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, palette.dim()))),
            area,
        );
        return;
    }

    let (start, visible) = window(area.height as usize, count, selected);
    picker.set_list_geometry(area, start);

    let now = Utc::now();
    let project_count = picker.entries().len();
    let lines: Vec<Line> = (start..start + visible)
        .map(|position| {
            if position < project_count {
                project_row(picker, position, position == selected, home, now, palette)
            } else {
                home_row(picker, position, position == selected, home, palette)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn project_row(
    picker: &Picker,
    position: usize,
    selected: bool,
    home: Option<&Path>,
    now: DateTime<Utc>,
    palette: &Palette,
) -> Line<'static> {
    let project = &picker.projects()[picker.entries()[position]];
    let missing = picker.is_missing(position);
    let marked = picker.marks().contains(&project.id);

    let state = if project.pinned {
        Span::styled("* ", palette.fg(Color::Yellow))
    } else if missing {
        Span::styled("! ", palette.fg(Color::Red))
    } else {
        Span::styled("  ", palette.dim())
    };
    let mark = if marked {
        Span::styled("▌ ", palette.fg(Color::Cyan))
    } else {
        Span::raw("  ")
    };

    let indices = matcher::highlight_indices(picker.query(), &project.name);
    let mut spans = vec![state, mark];
    spans.extend(highlighted(&project.name, &indices, palette.accent()));

    let mut detail = format!("  {}", output::shorten_home(&project.path, home));
    if let Some(when) = project.last_used_at {
        detail.push_str(&format!("  ·  {}", output::relative_time(when, now)));
    }
    if missing {
        detail.push_str("  (missing)");
    }
    spans.push(Span::styled(
        detail,
        if missing {
            palette.fg(Color::Red)
        } else {
            palette.dim()
        },
    ));

    let line = Line::from(spans);
    if selected {
        line.style(Style::default().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

/// A directory found under `$HOME`: not indexed, so the marker is `~` and
/// the row says `home`; Enter still jumps there.
fn home_row(
    picker: &Picker,
    position: usize,
    selected: bool,
    home: Option<&Path>,
    palette: &Palette,
) -> Line<'static> {
    let Some(found) = picker
        .home_matches()
        .get(position.saturating_sub(picker.entries().len()))
    else {
        return Line::default();
    };

    let indices = matcher::highlight_indices(picker.query(), &found.name);
    let mut spans = vec![Span::styled("~ ", palette.fg(Color::Blue))];
    spans.extend(highlighted(&found.name, &indices, palette.accent()));
    spans.push(Span::styled(
        format!("  {}  ·  home", output::shorten_home(&found.path, home)),
        palette.dim(),
    ));

    let line = Line::from(spans);
    if selected {
        line.style(Style::default().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

fn draw_palette(frame: &mut Frame, area: Rect, picker: &mut Picker, palette: &Palette) {
    let matches = picker.palette_matches();
    let selected = picker.palette().selected;

    if matches.is_empty() {
        picker.set_list_geometry(area, 0);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no matching action",
                palette.dim(),
            ))),
            area,
        );
        return;
    }

    let (start, visible) = window(area.height as usize, matches.len(), selected);
    picker.set_list_geometry(area, start);

    let lines: Vec<Line> = (start..start + visible)
        .map(|position| {
            let action = matches[position];
            let hint = action.hint();
            let marker = if position == selected { "▸ " } else { "  " };
            let line = Line::from(vec![
                Span::styled(marker.to_owned(), palette.accent()),
                Span::styled(action.label(), Style::default()),
                Span::styled(format!("  {hint}"), palette.dim()),
            ]);
            if position == selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_help(frame: &mut Frame, area: Rect, picker: &mut Picker, palette: &Palette) {
    picker.set_list_geometry(area, 0);
    let lines: Vec<Line> = HELP
        .iter()
        .take(area.height as usize)
        .map(|(keys, description)| {
            Line::from(vec![
                Span::styled(format!("  {keys:<14}"), palette.accent()),
                Span::styled(*description, Style::default()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_health(frame: &mut Frame, area: Rect, picker: &mut Picker, palette: &Palette) {
    let count = picker.issues().len();
    let selected = picker.health_selected();
    if count == 0 {
        picker.set_list_geometry(area, 0);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("no problems found", palette.dim()))),
            area,
        );
        return;
    }

    let (start, visible) = window(area.height as usize, count, selected);
    picker.set_list_geometry(area, start);
    let lines: Vec<Line> = (start..start + visible)
        .map(|position| {
            let row = &picker.issues()[position];
            let marker = if row.stale.is_some() { "! " } else { "  " };
            let mut line = Line::from(vec![
                Span::styled(
                    marker.to_owned(),
                    if row.stale.is_some() {
                        palette.fg(Color::Red)
                    } else {
                        palette.dim()
                    },
                ),
                Span::styled(row.label.clone(), Style::default()),
            ]);
            if position == selected {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_details(
    frame: &mut Frame,
    area: Rect,
    picker: &Picker,
    home: Option<&Path>,
    palette: &Palette,
) {
    let Some(project) = picker.selected_project() else {
        return;
    };
    let info = picker.details_info();

    let path = output::shorten_home(&project.path, home);
    let mut parts: Vec<String> = Vec::new();
    if !project.aliases.is_empty() {
        parts.push(format!(
            "aliases: {}",
            crate::sanitize::text(&project.aliases.join(", "))
        ));
    }
    if !project.tags.is_empty() {
        parts.push(format!(
            "tags: {}",
            crate::sanitize::text(&project.tags.join(", "))
        ));
    }
    if let Some(url) = project
        .git
        .as_ref()
        .and_then(|git| git.remote_url.as_deref())
        && let Some((owner, repo)) = crate::git::slug_from_url(url)
    {
        parts.push(crate::sanitize::text(&if owner.is_empty() {
            format!("repo: {repo}")
        } else {
            format!("repo: {owner}/{repo}")
        }));
    }
    parts.push(format!("uses: {}", project.use_count));

    let branch = info
        .filter(|info| info.id == project.id)
        .and_then(|info| info.branch.clone());
    let history = info
        .filter(|info| info.id == project.id)
        .map(|info| info.history.clone())
        .unwrap_or_default();

    let mut lines = vec![
        Line::from(Span::styled(path, palette.dim())),
        Line::from(Span::styled(parts.join("  ·  "), palette.dim())),
        Line::from(Span::styled(
            match branch {
                Some(branch) => format!("branch: {}", crate::sanitize::text(&branch)),
                None => String::new(),
            },
            palette.dim(),
        )),
    ];
    if !history.is_empty() {
        let rendered = output::format_times(&history, Utc::now());
        lines.push(Line::from(Span::styled(
            format!("last jumps: {rendered}"),
            palette.dim(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_input(frame: &mut Frame, area: Rect, picker: &Picker, palette: &Palette) {
    let (prompt, text, cursor) = match picker.mode() {
        Mode::Search | Mode::Nav => (
            "> ".to_owned(),
            picker.query().to_owned(),
            picker.mode() == Mode::Search,
        ),
        Mode::Prompt => (
            picker.prompt().kind.label().to_owned(),
            picker.prompt().input.clone(),
            true,
        ),
        Mode::Palette => (": ".to_owned(), picker.palette().query.clone(), true),
        Mode::Health => ("health".to_owned(), String::new(), false),
        Mode::Help => ("help".to_owned(), String::new(), false),
    };

    // The search box and prompt inputs are renderers too.
    let text = crate::sanitize::text(&text);
    let columns = Layout::horizontal([Constraint::Min(0), Constraint::Length(12)]).split(area);
    let line = Line::from(vec![
        Span::styled(prompt.clone(), palette.accent()),
        Span::raw(text.clone()),
    ]);
    frame.render_widget(Paragraph::new(line), columns[0]);

    let (position, count) = match picker.mode() {
        Mode::Palette => (
            picker.palette().selected + 1,
            picker.palette_matches().len(),
        ),
        Mode::Health => (picker.health_selected() + 1, picker.issues().len()),
        _ => (picker.selected() + 1, picker.total_len()),
    };
    let count = if count == 0 {
        "0/0".to_owned()
    } else if picker.sort() == crate::cli::SortBy::Used {
        format!("{position}/{count}")
    } else {
        // The active sort is worth showing once it is not the default.
        format!("{} · {position}/{count}", sort_label(picker.sort()))
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(count, palette.dim()))).alignment(Alignment::Right),
        columns[1],
    );

    if cursor {
        let offset =
            UnicodeWidthStr::width(prompt.as_str()) + UnicodeWidthStr::width(text.as_str());
        let x = area
            .x
            .saturating_add(offset as u16)
            .min(area.right().saturating_sub(1));
        frame.set_cursor_position((x, area.y));
    }
}

fn draw_status(frame: &mut Frame, area: Rect, picker: &Picker, palette: &Palette) {
    if let Some(confirm) = picker.confirm() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                crate::sanitize::text(&confirm.message),
                palette.fg(Color::Yellow),
            ))),
            area,
        );
        return;
    }

    let line = match picker.status() {
        Some(status) => Line::from(Span::styled(
            crate::sanitize::text(&status.text),
            if status.error {
                palette.fg(Color::Red)
            } else {
                palette.fg(Color::Green)
            },
        )),
        None => Line::from(Span::styled(hint(picker.mode()), palette.dim())),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn hint(mode: Mode) -> &'static str {
    match mode {
        Mode::Search => {
            "type to filter · ↑/↓ move · tab: list mode · enter jump · ^p pin · ^a add · ^x remove · ^space palette · esc cancel"
        }
        Mode::Nav => {
            "j/k move · g/G ends · m mark · enter jump · ^p pin · ^a add · ^x remove · : palette · ? help · esc cancel"
        }
        Mode::Prompt => "enter: apply · esc: back",
        Mode::Palette => "type to filter actions · j/k move · enter: run · esc: close",
        Mode::Help => "esc: back",
        Mode::Health => "j/k move · enter: relocate · x: remove · r: re-check · esc: back",
    }
}

/// The scrolling window `(start, rows)` around `selected` for `count` rows in
/// `height` lines.
fn window(height: usize, count: usize, selected: usize) -> (usize, usize) {
    if count <= height {
        return (0, count);
    }
    let start = selected
        .saturating_sub(height / 2)
        .min(count.saturating_sub(height));
    (start, height)
}

/// Splits `text` into `(chunk, matched)` runs so the matched characters can
/// be styled without breaking the rest apart.
fn highlight_runs(text: &str, indices: &[usize]) -> Vec<(String, bool)> {
    if indices.is_empty() {
        return vec![(text.to_owned(), false)];
    }
    let matched: HashSet<usize> = indices.iter().copied().collect();
    let mut runs: Vec<(String, bool)> = Vec::new();
    let mut chunk = String::new();
    let mut is_match = false;
    for (position, ch) in text.chars().enumerate() {
        let now = matched.contains(&position);
        if now != is_match {
            if !chunk.is_empty() {
                runs.push((std::mem::take(&mut chunk), is_match));
            }
            is_match = now;
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        runs.push((chunk, is_match));
    }
    runs
}

fn highlighted(text: &str, indices: &[usize], accent: Style) -> Vec<Span<'static>> {
    highlight_runs(text, indices)
        .into_iter()
        .map(|(chunk, matched)| {
            // Names can come from a planted index; the highlight indices align
            // with the raw text, so escape chunk by chunk.
            let chunk = crate::sanitize::text(&chunk);
            if matched {
                Span::styled(chunk, accent)
            } else {
                Span::raw(chunk)
            }
        })
        .collect()
}

struct Palette {
    color: bool,
}

impl Palette {
    fn accent(&self) -> Style {
        if self.color {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        }
    }

    fn dim(&self) -> Style {
        if self.color {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    /// A plain foreground color; colorless output resets it.
    fn fg(&self, color: Color) -> Style {
        Style::new().fg(if self.color { color } else { Color::Reset })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_runs_group_matched_characters() {
        assert_eq!(
            highlight_runs("overdosecd", &[0, 2, 3]),
            vec![
                ("o".to_owned(), true),
                ("v".to_owned(), false),
                ("er".to_owned(), true),
                ("dosecd".to_owned(), false),
            ]
        );
        assert_eq!(highlight_runs("ods", &[]), vec![("ods".to_owned(), false)]);
        assert_eq!(
            highlight_runs("abc", &[0, 1, 2]),
            vec![("abc".to_owned(), true)]
        );
    }

    #[test]
    fn scrolling_window_centers_and_clamps() {
        assert_eq!(window(10, 3, 0), (0, 3), "everything fits");
        assert_eq!(window(2, 100, 0), (0, 2));
        assert_eq!(
            window(2, 100, 50),
            (49, 2),
            "height 2 cannot center; the selection stays visible"
        );
        assert_eq!(window(2, 100, 99), (98, 2), "clamped to the end");
    }

    #[test]
    fn hints_cover_every_mode() {
        for mode in [
            Mode::Search,
            Mode::Nav,
            Mode::Prompt,
            Mode::Palette,
            Mode::Help,
            Mode::Health,
        ] {
            assert!(!hint(mode).is_empty(), "{mode:?}");
        }
    }
}
