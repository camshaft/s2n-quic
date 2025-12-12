// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::shared_state::SharedState;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph, Tabs},
    Frame, Terminal,
};
use std::{
    io,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Goodput,
    Latency,
    PacketLoss,
    Errors,
    Logs,
}

impl Tab {
    fn title(&self) -> &str {
        match self {
            Tab::Overview => "Overview",
            Tab::Goodput => "Goodput",
            Tab::Latency => "Latency",
            Tab::PacketLoss => "Packet Loss",
            Tab::Errors => "Errors",
            Tab::Logs => "Logs",
        }
    }

    fn all() -> &'static [Tab] {
        &[
            Tab::Overview,
            Tab::Goodput,
            Tab::Latency,
            Tab::PacketLoss,
            Tab::Errors,
            Tab::Logs,
        ]
    }

    fn next(&self) -> Tab {
        let tabs = Self::all();
        let current = tabs.iter().position(|t| t == self).unwrap();
        tabs[(current + 1) % tabs.len()]
    }

    fn previous(&self) -> Tab {
        let tabs = Self::all();
        let current = tabs.iter().position(|t| t == self).unwrap();
        tabs[(current + tabs.len() - 1) % tabs.len()]
    }
}

pub struct Tui {
    current_tab: Tab,
    state: SharedState,
    last_update: Instant,
}

impl Tui {
    pub fn new(state: SharedState) -> Self {
        Self {
            current_tab: Tab::Overview,
            state,
            last_update: Instant::now(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Run the TUI loop
        let result = self.run_loop(&mut terminal).await;

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        result
    }

    async fn run_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<()> {
        let tick_rate = Duration::from_millis(100);
        let mut last_tick = Instant::now();

        loop {
            terminal.draw(|f| self.draw(f))?;

            // Handle input with timeout
            let timeout = tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or_else(|| Duration::from_secs(0));

            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Tab | KeyCode::Right => {
                            self.current_tab = self.current_tab.next();
                        }
                        KeyCode::BackTab | KeyCode::Left => {
                            self.current_tab = self.current_tab.previous();
                        }
                        _ => {}
                    }
                }
            }

            if last_tick.elapsed() >= tick_rate {
                self.on_tick();
                last_tick = Instant::now();
            }
        }
    }

    fn on_tick(&mut self) {
        self.last_update = Instant::now();
        // Update any time-based metrics here
    }

    fn draw(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
            .split(f.area());

        // Render tabs
        self.render_tabs(f, chunks[0]);

        // Render current tab content
        match self.current_tab {
            Tab::Overview => self.render_overview(f, chunks[1]),
            Tab::Goodput => self.render_goodput(f, chunks[1]),
            Tab::Latency => self.render_latency(f, chunks[1]),
            Tab::PacketLoss => self.render_packet_loss(f, chunks[1]),
            Tab::Errors => self.render_errors(f, chunks[1]),
            Tab::Logs => self.render_logs(f, chunks[1]),
        }
    }

    fn render_tabs(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let titles: Vec<Line> = Tab::all().iter().map(|t| Line::from(t.title())).collect();

        let current_index = Tab::all()
            .iter()
            .position(|t| t == &self.current_tab)
            .unwrap();

        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("s2n-quic-dc CLI"),
            )
            .select(current_index)
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );

        f.render_widget(tabs, area);
    }

    fn render_logs(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let logs = self.state.get_logs();
        let log_text: Vec<Line> = logs
            .iter()
            .rev()
            .take(area.height as usize - 2)
            .rev()
            .map(|log| Line::from(log.as_str()))
            .collect();

        let paragraph = Paragraph::new(log_text)
            .block(Block::default().borders(Borders::ALL).title("Logs"))
            .style(Style::default().fg(Color::White));

        f.render_widget(paragraph, area);
    }

    fn render_goodput(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let history = self.state.get_goodput_history();

        if history.is_empty() {
            let text = vec![
                Line::from(Span::styled(
                    "Goodput Percentage",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("Waiting for data..."),
            ];
            let paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title("Goodput"))
                .style(Style::default().fg(Color::White));
            f.render_widget(paragraph, area);
            return;
        }

        // Convert history to chart data points
        let start_time = history
            .first()
            .map(|(t, _)| *t)
            .unwrap_or(std::time::Instant::now());
        let data: Vec<(f64, f64)> = history
            .iter()
            .map(|(time, value)| {
                let seconds = time.duration_since(start_time).as_secs_f64();
                (seconds, *value)
            })
            .collect();

        let min_value = data
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::INFINITY, f64::min)
            .min(0.0);
        let max_value = data
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::NEG_INFINITY, f64::max)
            .max(100.0);

        let max_x = data.last().map(|(x, _)| *x).unwrap_or(30.0);

        let datasets = vec![Dataset::default()
            .name("Goodput %")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
            .data(&data)];

        let current_value = self
            .state
            .get_latest_metrics()
            .map(|m| format!("Current: {:.2}%", m.goodput_pct))
            .unwrap_or_default();

        let chart = Chart::new(datasets)
            .block(
                Block::default()
                    .title(format!("Goodput - {}", current_value))
                    .borders(Borders::ALL),
            )
            .x_axis(
                Axis::default()
                    .title("Time (s)")
                    .style(Style::default().fg(Color::Gray))
                    .bounds([0.0, max_x])
                    .labels(vec![
                        Span::raw("0"),
                        Span::raw(format!("{:.0}", max_x / 2.0)),
                        Span::raw(format!("{:.0}", max_x)),
                    ]),
            )
            .y_axis(
                Axis::default()
                    .title("%")
                    .style(Style::default().fg(Color::Gray))
                    .bounds([min_value, max_value])
                    .labels(vec![
                        Span::raw(format!("{:.0}", min_value)),
                        Span::raw(format!("{:.0}", (min_value + max_value) / 2.0)),
                        Span::raw(format!("{:.0}", max_value)),
                    ]),
            );

        f.render_widget(chart, area);
    }

    fn render_latency(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let history = self.state.get_latency_history();

        if history.is_empty() {
            let text = vec![
                Line::from(Span::styled(
                    "Average Latency",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("Waiting for data..."),
            ];
            let paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title("Latency"))
                .style(Style::default().fg(Color::White));
            f.render_widget(paragraph, area);
            return;
        }

        // Convert history to chart data points
        let start_time = history
            .first()
            .map(|(t, _)| *t)
            .unwrap_or(std::time::Instant::now());
        let data: Vec<(f64, f64)> = history
            .iter()
            .map(|(time, value)| {
                let seconds = time.duration_since(start_time).as_secs_f64();
                (seconds, *value)
            })
            .collect();

        let min_value = data
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::INFINITY, f64::min)
            .min(0.0);
        let max_value = data
            .iter()
            .map(|(_, v)| *v)
            .fold(f64::NEG_INFINITY, f64::max)
            .max(1.0);

        let max_x = data.last().map(|(x, _)| *x).unwrap_or(30.0);

        let datasets = vec![Dataset::default()
            .name("Latency (ms)")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&data)];

        let current_value = self
            .state
            .get_latest_metrics()
            .map(|m| format!("Current: {:.2} ms", m.avg_latency_ms))
            .unwrap_or_default();

        let chart = Chart::new(datasets)
            .block(
                Block::default()
                    .title(format!("Latency - {}", current_value))
                    .borders(Borders::ALL),
            )
            .x_axis(
                Axis::default()
                    .title("Time (s)")
                    .style(Style::default().fg(Color::Gray))
                    .bounds([0.0, max_x])
                    .labels(vec![
                        Span::raw("0"),
                        Span::raw(format!("{:.0}", max_x / 2.0)),
                        Span::raw(format!("{:.0}", max_x)),
                    ]),
            )
            .y_axis(
                Axis::default()
                    .title("ms")
                    .style(Style::default().fg(Color::Gray))
                    .bounds([min_value, max_value])
                    .labels(vec![
                        Span::raw(format!("{:.1}", min_value)),
                        Span::raw(format!("{:.1}", (min_value + max_value) / 2.0)),
                        Span::raw(format!("{:.1}", max_value)),
                    ]),
            );

        f.render_widget(chart, area);
    }

    fn render_packet_loss(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let history = self.state.get_loss_history();

        if history.is_empty() {
            let text = vec![
                Line::from(Span::styled(
                    "Packet Loss Rate",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("Waiting for data..."),
            ];
            let paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title("Packet Loss"))
                .style(Style::default().fg(Color::White));
            f.render_widget(paragraph, area);
            return;
        }

        // Convert history to chart data points
        let start_time = history
            .first()
            .map(|(t, _)| *t)
            .unwrap_or(std::time::Instant::now());
        let data: Vec<(f64, f64)> = history
            .iter()
            .map(|(time, value)| {
                let seconds = time.duration_since(start_time).as_secs_f64();
                (seconds, *value)
            })
            .collect();

        let min_value = 0.0;
        let max_value = data
            .iter()
            .map(|(_, v)| *v)
            .fold(0.0_f64, f64::max)
            .max(1.0); // At least 1% scale

        let max_x = data.last().map(|(x, _)| *x).unwrap_or(30.0);

        let datasets = vec![Dataset::default()
            .name("Loss %")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Red))
            .data(&data)];

        let current_value = self
            .state
            .get_latest_metrics()
            .map(|m| format!("Current: {:.2}%", m.loss_rate_pct))
            .unwrap_or_default();

        let chart = Chart::new(datasets)
            .block(
                Block::default()
                    .title(format!("Packet Loss - {}", current_value))
                    .borders(Borders::ALL),
            )
            .x_axis(
                Axis::default()
                    .title("Time (s)")
                    .style(Style::default().fg(Color::Gray))
                    .bounds([0.0, max_x])
                    .labels(vec![
                        Span::raw("0"),
                        Span::raw(format!("{:.0}", max_x / 2.0)),
                        Span::raw(format!("{:.0}", max_x)),
                    ]),
            )
            .y_axis(
                Axis::default()
                    .title("%")
                    .style(Style::default().fg(Color::Gray))
                    .bounds([min_value, max_value])
                    .labels(vec![
                        Span::raw(format!("{:.1}", min_value)),
                        Span::raw(format!("{:.1}", max_value / 2.0)),
                        Span::raw(format!("{:.1}", max_value)),
                    ]),
            );

        f.render_widget(chart, area);
    }

    fn render_errors(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let stream_errors = self.state.get_stream_errors();
        let panics = self.state.get_panics();

        let error_color = if stream_errors > 0 || !panics.is_empty() {
            Color::Red
        } else {
            Color::Green
        };

        let mut text = vec![
            Line::from(Span::styled(
                "Error Statistics",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "Stream Errors: ",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{}", stream_errors),
                    Style::default()
                        .fg(if stream_errors > 0 {
                            Color::Red
                        } else {
                            Color::Green
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Panics: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("{}", panics.len()),
                    Style::default()
                        .fg(if panics.is_empty() {
                            Color::Green
                        } else {
                            Color::Red
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
        ];

        if stream_errors == 0 && panics.is_empty() {
            text.push(Line::from(Span::styled(
                "✓ No errors detected",
                Style::default().fg(Color::Green),
            )));
        } else {
            if !panics.is_empty() {
                text.push(Line::from("Recent Panics:"));
                for (i, panic_msg) in panics.iter().rev().take(5).enumerate() {
                    text.push(Line::from(Span::styled(
                        format!("  #{}: {}", panics.len() - i, panic_msg),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }

        let paragraph = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled("Errors", Style::default().fg(error_color))),
            )
            .style(Style::default().fg(Color::White));

        f.render_widget(paragraph, area);
    }

    fn render_overview(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        let status = self.state.get_status();
        let (active, total) = self.state.get_connection_stats();
        let stream_errors = self.state.get_stream_errors();
        let uptime = self.state.get_uptime();

        // Calculate success rate
        let success_rate = if total > 0 {
            ((total - stream_errors) as f64 / total as f64) * 100.0
        } else {
            100.0
        };

        let success_color = if success_rate >= 99.0 {
            Color::Green
        } else if success_rate >= 95.0 {
            Color::Yellow
        } else {
            Color::Red
        };

        let mut text = vec![
            Line::from(Span::styled(
                "s2n-quic-dc CLI - Overview",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("Status: "),
                Span::styled(
                    &status,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(format!("Uptime: {:?}", uptime)),
            Line::from(""),
            Line::from(vec![Span::styled(
                "System Health:",
                Style::default().add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::raw("  Success Rate: "),
                Span::styled(
                    format!("{:.2}%", success_rate),
                    Style::default()
                        .fg(success_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  ({}/{} streams)", total - stream_errors, total)),
            ]),
            Line::from(""),
            Line::from(vec![Span::styled(
                "Connections:",
                Style::default().add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!("  Active: {}", active)),
            Line::from(format!("  Total:  {}", total)),
            Line::from(format!("  Errors: {}", stream_errors)),
            Line::from(""),
        ];

        if let Some(metrics) = self.state.get_latest_metrics() {
            text.push(Line::from(vec![Span::styled(
                "Metrics:",
                Style::default().add_modifier(Modifier::BOLD),
            )]));
            text.push(Line::from(vec![
                Span::raw("  Goodput:     "),
                Span::styled(
                    format!("{:.2}%", metrics.goodput_pct),
                    Style::default().fg(Color::Green),
                ),
            ]));
            text.push(Line::from(vec![
                Span::raw("  Avg Latency: "),
                Span::styled(
                    format!("{:.2} ms", metrics.avg_latency_ms),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            text.push(Line::from(vec![
                Span::raw("  Packet Loss: "),
                Span::styled(
                    format!("{:.2}%", metrics.loss_rate_pct),
                    Style::default().fg(if metrics.loss_rate_pct > 1.0 {
                        Color::Red
                    } else {
                        Color::Green
                    }),
                ),
            ]));
        } else {
            text.push(Line::from("Waiting for metrics..."));
        }

        text.push(Line::from(""));
        text.push(Line::from("Controls:"));
        text.push(Line::from("  Tab / Right Arrow  - Next tab"));
        text.push(Line::from("  Shift+Tab / Left   - Previous tab"));
        text.push(Line::from("  q / Esc            - Quit"));

        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("Overview"))
            .style(Style::default().fg(Color::White));

        f.render_widget(paragraph, area);
    }
}
