//! The interactive install wizard (ratatui).
//!
//! A small state machine — Preset → Adapters → Roots → Review → Plan — that
//! collects the user's choices, shows the generated config (with a diff against
//! any existing one), and returns a decided [`plan`](crate::plan) for the
//! caller to execute *after* the terminal is restored.

use std::io;
use std::path::PathBuf;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::adapters::{ADAPTERS, Choices, Preset, render_config};
use crate::diff::diff_lines;
use crate::plan::{self, Action, Conflict, Decided};

/// What the wizard returns.
pub enum Outcome {
    /// Proceed with these decided actions.
    Install(Vec<Decided>),
    /// User cancelled — change nothing.
    Cancelled,
}

enum Step {
    Preset,
    Adapters,
    Roots,
    Review,
    Plan,
}

/// The wizard state.
pub struct Wizard {
    home: PathBuf,
    bin: PathBuf,
    step: Step,
    choices: Choices,
    preset_idx: usize,
    adapter_idx: usize,
    review_scroll: u16,
    show_diff: bool,
    config_text: String,
    existing_config: Option<String>,
    actions: Vec<Action>,
    skip: Vec<bool>,
    plan_idx: usize,
}

impl Wizard {
    /// Create a wizard rooted at `home`, installing the located `bin`.
    pub fn new(home: PathBuf, bin: PathBuf) -> Self {
        let existing_config = std::fs::read_to_string(home.join(".disk-saver.toml")).ok();
        Wizard {
            home,
            bin,
            step: Step::Preset,
            choices: Choices::defaults(),
            preset_idx: 0,
            adapter_idx: 0,
            review_scroll: 0,
            show_diff: existing_config.is_some(),
            config_text: String::new(),
            existing_config,
            actions: Vec::new(),
            skip: Vec::new(),
            plan_idx: 0,
        }
    }

    /// Drive the event loop until the user installs or cancels.
    pub fn run(mut self, terminal: &mut DefaultTerminal) -> io::Result<Outcome> {
        loop {
            terminal.draw(|frame| self.render(frame))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(outcome) = self.handle(key) {
                return Ok(outcome);
            }
        }
    }

    // ── event handling ──────────────────────────────────────────────────────

    fn handle(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Outcome::Cancelled);
        }
        match self.step {
            Step::Preset => self.handle_preset(key),
            Step::Adapters => self.handle_adapters(key),
            Step::Roots => self.handle_roots(key),
            Step::Review => self.handle_review(key),
            Step::Plan => self.handle_plan(key),
        }
    }

    fn handle_preset(&mut self, key: KeyEvent) -> Option<Outcome> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.preset_idx = self.preset_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.preset_idx = (self.preset_idx + 1).min(Preset::ALL.len() - 1);
            }
            KeyCode::Enter => {
                self.choices.preset = Preset::ALL[self.preset_idx];
                self.step = Step::Adapters;
            }
            KeyCode::Esc | KeyCode::Char('q') => return Some(Outcome::Cancelled),
            _ => {}
        }
        None
    }

    fn handle_adapters(&mut self, key: KeyEvent) -> Option<Outcome> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.adapter_idx = self.adapter_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.adapter_idx = (self.adapter_idx + 1).min(ADAPTERS.len() - 1);
            }
            KeyCode::Char(' ') => {
                let e = &mut self.choices.enabled[self.adapter_idx];
                *e = !*e;
            }
            KeyCode::Enter => self.step = Step::Roots,
            KeyCode::Char('b') => self.step = Step::Preset,
            KeyCode::Esc | KeyCode::Char('q') => return Some(Outcome::Cancelled),
            _ => {}
        }
        None
    }

    fn handle_roots(&mut self, key: KeyEvent) -> Option<Outcome> {
        match key.code {
            KeyCode::Char(c) => self.choices.roots.push(c),
            KeyCode::Backspace => {
                self.choices.roots.pop();
            }
            KeyCode::Enter => {
                self.config_text = render_config(&self.choices);
                self.review_scroll = 0;
                self.step = Step::Review;
            }
            KeyCode::Esc => self.step = Step::Adapters,
            _ => {}
        }
        None
    }

    fn handle_review(&mut self, key: KeyEvent) -> Option<Outcome> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.review_scroll = self.review_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.review_scroll = self.review_scroll.saturating_add(1);
            }
            KeyCode::Char('d') if self.existing_config.is_some() => {
                self.show_diff = !self.show_diff
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                self.actions = plan::build(self.config_text.clone(), &self.bin, &self.home);
                self.skip = vec![false; self.actions.len()];
                self.plan_idx = 0;
                self.step = Step::Plan;
            }
            KeyCode::Char('b') | KeyCode::Esc => self.step = Step::Roots,
            _ => {}
        }
        None
    }

    fn handle_plan(&mut self, key: KeyEvent) -> Option<Outcome> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.plan_idx = self.plan_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.plan_idx = (self.plan_idx + 1).min(self.actions.len().saturating_sub(1));
            }
            KeyCode::Char(' ') => {
                if let Some(s) = self.skip.get_mut(self.plan_idx) {
                    *s = !*s;
                }
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let decided = std::mem::take(&mut self.actions)
                    .into_iter()
                    .zip(&self.skip)
                    .map(|(action, &skip)| Decided {
                        action,
                        apply: !skip,
                    })
                    .collect();
                return Some(Outcome::Install(decided));
            }
            KeyCode::Char('b') => self.step = Step::Review,
            KeyCode::Esc | KeyCode::Char('q') => return Some(Outcome::Cancelled),
            _ => {}
        }
        None
    }

    // ── rendering ─────────────────────────────────────────────────────────────

    fn render(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .areas(frame.area());

        let (title, keys) = match self.step {
            Step::Preset => (
                "disk-saver installer — 1/5  Cleaning aggressiveness",
                "↑/↓ choose · enter next · q quit",
            ),
            Step::Adapters => (
                "disk-saver installer — 2/5  Adapters",
                "↑/↓ move · space toggle · enter next · b back · q quit",
            ),
            Step::Roots => (
                "disk-saver installer — 3/5  Project roots to scan",
                "type to edit · enter next · esc back",
            ),
            Step::Review => (
                "disk-saver installer — 4/5  Review ~/.disk-saver.toml",
                "↑/↓ scroll · d diff · enter continue · b back",
            ),
            Step::Plan => (
                "disk-saver installer — 5/5  Install plan",
                "↑/↓ move · space skip · enter INSTALL · b back · q quit",
            ),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))),
            header,
        );

        match self.step {
            Step::Preset => self.render_preset(frame, body),
            Step::Adapters => self.render_adapters(frame, body),
            Step::Roots => self.render_roots(frame, body),
            Step::Review => self.render_review(frame, body),
            Step::Plan => self.render_plan(frame, body),
        }

        frame.render_widget(
            Paragraph::new(keys)
                .style(Style::default().fg(Color::DarkGray))
                .block(Block::default().borders(Borders::TOP)),
            footer,
        );
    }

    fn render_preset(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = Preset::ALL
            .iter()
            .map(|p| {
                ListItem::new(vec![Line::from(vec![
                    Span::styled(
                        format!("{:<20}", p.label()),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(p.desc(), Style::default().fg(Color::DarkGray)),
                ])])
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Preset "))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
            .highlight_symbol("▸ ");
        frame.render_stateful_widget(
            list,
            area,
            &mut ListState::default().with_selected(Some(self.preset_idx)),
        );
    }

    fn render_adapters(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = ADAPTERS
            .iter()
            .zip(&self.choices.enabled)
            .map(|(info, &on)| {
                let mark = if on { "[x]" } else { "[ ]" };
                let mark_style = if on {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark} "), mark_style),
                    Span::styled(
                        format!("{:<16}", info.name),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(info.desc, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Adapters (space to toggle) "),
            )
            .highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)))
            .highlight_symbol("▸ ");
        frame.render_stateful_widget(
            list,
            area,
            &mut ListState::default().with_selected(Some(self.adapter_idx)),
        );
    }

    fn render_roots(&mut self, frame: &mut Frame, area: Rect) {
        let body = vec![
            Line::from("Directories the filesystem and git adapters scan for projects."),
            Line::from(Span::styled(
                "Comma-separated. e.g.  ~/dev, ~/work   (blank = your whole home ~)",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("roots: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(&self.choices.roots),
                Span::styled("▏", Style::default().fg(Color::Cyan)),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Project roots "),
            ),
            area,
        );
    }

    fn render_review(&mut self, frame: &mut Frame, area: Rect) {
        let has_diff = self
            .existing_config
            .as_deref()
            .is_some_and(|e| e != self.config_text);

        let (title, lines): (String, Vec<Line>) = if self.show_diff && has_diff {
            (
                " Changes to ~/.disk-saver.toml (existing → new) ".to_string(),
                diff_lines(
                    self.existing_config.as_deref().unwrap_or(""),
                    &self.config_text,
                ),
            )
        } else {
            let note = if has_diff {
                "  (press d to diff against your existing config)"
            } else if self.existing_config.is_some() {
                "  (identical to your existing config)"
            } else {
                "  (new file)"
            };
            (
                format!(" ~/.disk-saver.toml{note} "),
                self.config_text.lines().map(Line::from).collect(),
            )
        };

        frame.render_widget(
            Paragraph::new(lines)
                .scroll((self.review_scroll, 0))
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    fn render_plan(&mut self, frame: &mut Frame, area: Rect) {
        let [list_area, detail_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(4)]).areas(area);

        let items: Vec<ListItem> = self
            .actions
            .iter()
            .zip(&self.skip)
            .map(|(action, &skip)| {
                let (mark, style) = if skip {
                    ("[skip]", Style::default().fg(Color::DarkGray))
                } else {
                    ("[ ok ]", Style::default().fg(Color::Green))
                };
                let mut spans = vec![
                    Span::styled(format!("{mark} "), style),
                    Span::raw(action.summary()),
                ];
                if let Conflict::Overwrite { path } = action.conflict() {
                    spans.push(Span::styled(
                        format!("  (overwrites {}, backed up)", plan::home_rel(&path)),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Plan (space to skip a step) "),
            )
            .highlight_style(Style::default().bg(Color::Rgb(40, 40, 40)))
            .highlight_symbol("▸ ");
        frame.render_stateful_widget(
            list,
            list_area,
            &mut ListState::default().with_selected(Some(self.plan_idx)),
        );

        let detail = self
            .actions
            .get(self.plan_idx)
            .map(Action::detail)
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::DarkGray))
                .block(Block::default().borders(Borders::ALL).title(" Detail ")),
            detail_area,
        );
    }
}
