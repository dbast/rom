//! Terminal admission and atomic normal-screen composition.

use std::{
  io::{self, IsTerminal, Write},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use anstyle_progress::TermProgress;
use crossterm::{
  cursor::{Hide, MoveToColumn, MoveToPreviousLine, Show},
  queue,
  terminal::{Clear, ClearType},
};

use crate::{
  display::{MIN_TREE_WIDTH, render_frame, write_final},
  state::{State, elapsed},
  types::{DisplayFormat, ProgressMode, RenderConfig},
};

const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
const END_SYNC: &[u8] = b"\x1b[?2026l";
const MIN_LIVE_COLUMNS: u16 = 20;
const MIN_LIVE_ROWS: u16 = 8;
const MAX_PENDING_BYTES: usize = 64 * 1024;
const PARTIAL_LINE_GRACE: f64 = 0.5;
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const MAX_FRAME_INTERVAL: Duration = Duration::from_secs(1);

/// Why live presentation was or was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
  /// Live redraws are allowed.
  Live,
  /// Standard error is not a terminal.
  NotATerminal,
  /// The terminal is inside tmux or GNU Screen.
  Multiplexer,
}

/// Selects live redraws for every direct interactive terminal.
///
/// Like nix-output-monitor, ROM sends synchronized-update markers without a
/// capability round trip. Terminals which do not implement mode 2026 ignore
/// the private mode while still understanding the ordinary cursor operations.
/// Multiplexers remain append-only until their passthrough behavior has an
/// explicit compatibility test.
#[must_use]
pub fn admission() -> Admission {
  admission_from(
    io::stderr().is_terminal(),
    std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some(),
  )
}

const fn admission_from(is_terminal: bool, multiplexer: bool) -> Admission {
  if !is_terminal {
    Admission::NotATerminal
  } else if multiplexer {
    Admission::Multiplexer
  } else {
    Admission::Live
  }
}

/// Counts the newlines in `bytes`.
#[expect(
  clippy::naive_bytecount,
  reason = "pending logs are small, so a dependency would not pay off"
)]
fn count_lines(bytes: &[u8]) -> usize {
  bytes.iter().filter(|&&byte| byte == b'\n').count()
}

/// Owns the live graph region and serializes all terminal output.
///
/// OSC 9;4 progress is automatically enabled on supported direct stderr
/// terminals. `RenderConfig::progress` selects auto, always, or never; in auto
/// mode `ROM_PROGRESS` overrides detection (1/true/always or 0/false/never).
/// This permits enabling over SSH. Pipes, `TERM=dumb`,
/// and multiplexers are excluded. Progress is independent of ANSI colors.
#[expect(
  clippy::struct_excessive_bools,
  reason = "the flags are independent lifecycle facts"
)]
pub struct LiveTerminal<W: Write> {
  writer:               W,
  graph_height:         u16,
  initialized:          bool,
  retired:              bool,
  finished:             bool,
  pending:              Vec<u8>,
  partial_line:         bool,
  partial_since:        Option<f64>,
  frame:                String,
  frame_cost:           Duration,
  progress_enabled:     Option<bool>,
  progress:             Option<ProgressIndicator>,
  progress_outstanding: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProgressIndicator {
  Indeterminate,
  Percent(u8),
}

impl<W: Write> LiveTerminal<W> {
  /// Creates a live terminal that writes to `writer`.
  #[must_use]
  pub const fn new(writer: W) -> Self {
    Self {
      writer,
      graph_height: 0,
      initialized: false,
      retired: false,
      finished: false,
      pending: Vec::new(),
      partial_line: false,
      partial_since: None,
      frame: String::new(),
      frame_cost: Duration::ZERO,
      progress_enabled: None,
      progress: None,
      progress_outstanding: None,
    }
  }

  #[cfg(feature = "cli")]
  pub(crate) fn track_progress(&mut self, outstanding: Arc<AtomicBool>) {
    self.progress_outstanding = Some(outstanding);
  }

  /// Returns whether live rendering has stopped for good.
  #[must_use]
  pub const fn is_retired(&self) -> bool {
    self.retired
  }

  /// Returns the wait before the next frame, scaled to the last frame's cost.
  #[must_use]
  pub fn frame_interval(&self) -> Duration {
    (self.frame_cost * 10).clamp(MIN_FRAME_INTERVAL, MAX_FRAME_INTERVAL)
  }

  /// Renders one frame, retrying a bounded number of times if the terminal is
  /// resized while the synchronized transaction is still being composed.
  ///
  /// Returns `Ok(false)` without drawing when live rendering has retired or a
  /// partial log line is pending.
  ///
  /// # Errors
  ///
  /// Returns an error if the terminal size cannot be queried or the frame
  /// cannot be written to the terminal.
  pub fn render(
    &mut self,
    state: &State,
    config: &RenderConfig,
    now: f64,
    final_render: bool,
  ) -> io::Result<bool> {
    self.update_progress(state, config, final_render)?;
    if self.retired {
      return Ok(false);
    }
    let mut complete = complete_lines(&self.pending);
    if final_render && complete < self.pending.len() {
      self.pending.extend_from_slice(b"\r\n");
      complete = self.pending.len();
    }
    if self.partial_line {
      if complete == 0 {
        return Ok(false);
      }
      self.partial_line = false;
    }
    if complete == self.pending.len() {
      self.partial_since = None;
    } else {
      let since = *self.partial_since.get_or_insert(now);
      if elapsed(since, now) >= PARTIAL_LINE_GRACE {
        self.partial_since = None;
        self.partial_line = true;
        self.clear_graph()?;
        self.writer.flush()?;
        return Ok(false);
      }
    }
    for _ in 0..3 {
      let begin = Instant::now();
      let (columns, rows) = crossterm::terminal::size()?;
      // The renderer receives one fewer column than the terminal reserves.
      let tree_too_narrow = matches!(config.format, DisplayFormat::Tree)
        && columns <= MIN_TREE_WIDTH;
      if !live_geometry(columns, rows) || tree_too_narrow {
        self.retire()?;
        return Ok(false);
      }
      let frame =
        render_frame(state, config, now, columns - 1, rows - 1, final_render);
      // A shorter frame would leave blank rows under it, lifting the graph off
      // the bottom of the screen. Keep the region's height until new log lines
      // take over the freed rows; the final frame needs no anchoring.
      let logs = &self.pending[..complete];
      let log_lines = count_lines(logs);
      let reserved = if final_render {
        0
      } else {
        usize::from(self.graph_height).saturating_sub(log_lines)
      };
      let padding = reserved.saturating_sub(usize::from(frame.height));
      let mut text = "\r\n".repeat(padding);
      text.push_str(&frame.ansi_text().replace('\n', "\r\n"));
      let bytes = self.compose(logs, &text)?;
      if crossterm::terminal::size()? != (columns, rows) {
        continue;
      }
      self.writer.write_all(&bytes)?;
      self.writer.flush()?;
      self.pending.drain(..complete);
      self.frame = text;
      self.graph_height = (frame.height + padding as u16).min(rows - 1);
      self.initialized = true;
      self.frame_cost = begin.elapsed();
      return Ok(true);
    }
    self.retire()?;
    Ok(false)
  }

  fn update_progress(
    &mut self,
    state: &State,
    config: &RenderConfig,
    final_render: bool,
  ) -> io::Result<()> {
    if final_render || self.finished {
      return self.clear_progress();
    }
    if self.progress_enabled.is_none() {
      let mode = config.progress.resolve()?;
      // Environment hints may be absent over SSH; an override still requires
      // a direct terminal, never a pipe or unvalidated multiplexer.
      self.progress_enabled = Some(
        admission() == Admission::Live
          && std::env::var("TERM").as_deref() != Ok("dumb")
          && match mode {
            ProgressMode::Never => false,
            ProgressMode::Always => true,
            ProgressMode::Auto => {
              anstyle_progress::supports_term_progress(true)
            },
          },
      );
    }
    if self.progress_enabled != Some(true) {
      return Ok(());
    }
    let builds = state.builds_progress();
    // This measures completed builds, not elapsed work. Return to busy when
    // builds finish but Nix is still transferring or doing other work.
    let percent =
      (builds.expected > 0 && builds.done < builds.expected).then(|| {
        ((u128::from(builds.done) * 100) / u128::from(builds.expected)) as u8
      });
    let indicator = percent
      .map_or(ProgressIndicator::Indeterminate, ProgressIndicator::Percent);
    if self.progress == Some(indicator) {
      return Ok(());
    }
    // TermProgress formats OSC 9;4, terminated by ST (ESC backslash).
    let report = percent.map_or_else(TermProgress::start, |value| {
      TermProgress::start().percent(value)
    });
    write!(self.writer, "{report}")?;
    self.progress = Some(indicator);
    if let Some(outstanding) = &self.progress_outstanding {
      outstanding.store(true, Ordering::SeqCst);
    }
    self.writer.flush()
  }

  fn clear_progress(&mut self) -> io::Result<()> {
    if self.progress.is_some() {
      write!(self.writer, "{}", TermProgress::remove())?;
      self.progress = None;
      self.writer.flush()?;
      if let Some(outstanding) = &self.progress_outstanding {
        outstanding.store(false, Ordering::SeqCst);
      }
    }
    Ok(())
  }

  fn compose(&self, logs: &[u8], frame: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(BEGIN_SYNC);
    queue!(bytes, Hide)?;
    self.queue_clear_graph(&mut bytes)?;
    bytes.extend_from_slice(logs);
    queue!(bytes, MoveToColumn(0))?;
    bytes.extend_from_slice(frame.as_bytes());
    queue!(bytes, Show)?;
    bytes.extend_from_slice(END_SYNC);
    Ok(bytes)
  }

  fn queue_clear_graph(&self, bytes: &mut Vec<u8>) -> io::Result<()> {
    if !self.initialized {
      return Ok(());
    }
    queue!(bytes, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    for _ in 1..self.graph_height {
      queue!(bytes, MoveToPreviousLine(1), Clear(ClearType::CurrentLine))?;
    }
    Ok(())
  }

  /// Buffers exact output bytes for the next atomic logs-plus-graph commit.
  ///
  /// This deliberately matches nix-output-monitor: ROM does not interpret or
  /// classify controls belonging to the producer.
  ///
  /// # Errors
  ///
  /// Returns an error if writing to the terminal fails, either directly after
  /// retirement or while flushing an overflowing buffer.
  pub fn write_passthrough(&mut self, input: &[u8]) -> io::Result<()> {
    if input.is_empty() {
      return Ok(());
    }
    if self.retired {
      self.writer.write_all(input)?;
      return self.writer.flush();
    }
    let mut offset = 0;
    while offset < input.len() {
      let count =
        (MAX_PENDING_BYTES - self.pending.len()).min(input.len() - offset);
      self
        .pending
        .extend_from_slice(&input[offset..offset + count]);
      offset += count;
      if self.pending.len() == MAX_PENDING_BYTES {
        self.overflow()?;
      }
    }
    Ok(())
  }

  /// Writes pending log lines above the last frame.
  ///
  /// # Errors
  ///
  /// Returns an error if the logs cannot be written to the terminal.
  pub fn flush_logs(&mut self) -> io::Result<()> {
    let complete = complete_lines(&self.pending);
    if self.retired || !self.initialized || complete == 0 {
      return Ok(());
    }

    let bytes = self.compose(&self.pending[..complete], &self.frame)?;
    self.partial_line = false;
    self.writer.write_all(&bytes)?;
    self.pending.drain(..complete);
    self.writer.flush()
  }

  fn overflow(&mut self) -> io::Result<()> {
    if !self.initialized || complete_lines(&self.pending) == 0 {
      self.partial_line = !self.pending.ends_with(b"\n");
      self.clear_graph()?;
      return self.writer.flush();
    }

    self.flush_logs()
  }

  /// Clears the live graph and permanently falls back to plain passthrough.
  ///
  /// # Errors
  ///
  /// Returns an error if clearing the graph or flushing the terminal fails.
  pub fn retire(&mut self) -> io::Result<()> {
    if self.retired {
      return Ok(());
    }
    self.retired = true;
    self.clear_graph()?;
    self.writer.flush()
  }

  fn clear_graph(&mut self) -> io::Result<()> {
    let mut bytes = Vec::new();
    if self.initialized {
      bytes.extend_from_slice(BEGIN_SYNC);
      queue!(bytes, Hide)?;
      self.queue_clear_graph(&mut bytes)?;
    }
    bytes.extend_from_slice(&self.pending);
    if self.initialized {
      queue!(bytes, Show)?;
      bytes.extend_from_slice(END_SYNC);
    }
    self.writer.write_all(&bytes)?;
    self.pending.clear();
    self.initialized = false;
    self.graph_height = 0;
    Ok(())
  }

  /// Leaves the final frame in normal scrollback and restores terminal modes.
  ///
  /// # Errors
  ///
  /// Returns an error if writing to the terminal fails.
  pub fn finish(&mut self) -> io::Result<()> {
    self.clear_progress()?;
    if self.finished {
      return Ok(());
    }
    if !self.retired && self.initialized {
      let mut bytes = Vec::new();
      bytes.extend_from_slice(BEGIN_SYNC);
      queue!(bytes, Show, MoveToColumn(0))?;
      bytes.extend_from_slice(b"\r\n");
      bytes.extend_from_slice(END_SYNC);
      self.writer.write_all(&bytes)?;
      self.writer.flush()?;
    }
    self.finished = true;
    Ok(())
  }

  /// Appends a final frame sized to the current terminal.
  ///
  /// Used when live rendering retired, for example because the window was too
  /// small.
  ///
  /// # Errors
  ///
  /// Returns an error if writing to the terminal fails.
  pub fn append_final(
    &mut self,
    state: &State,
    config: &RenderConfig,
    now: f64,
  ) -> io::Result<()> {
    let mut config = config.clone();
    if let Ok((columns, rows)) = crossterm::terminal::size() {
      config.width.get_or_insert(columns);
      config.height.get_or_insert(rows);
    }
    write_final(self, state, &config, now)
  }
}

fn complete_lines(bytes: &[u8]) -> usize {
  bytes
    .iter()
    .rposition(|&byte| byte == b'\n')
    .map_or(0, |index| index + 1)
}

const fn live_geometry(columns: u16, rows: u16) -> bool {
  columns >= MIN_LIVE_COLUMNS && rows >= MIN_LIVE_ROWS
}

impl<W: Write> Drop for LiveTerminal<W> {
  fn drop(&mut self) {
    let _ = self.clear_progress();
    if !self.finished && !self.retired {
      let _ = self.retire();
    }
  }
}

impl<W: Write> Write for LiveTerminal<W> {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.write_passthrough(buf)?;
    Ok(buf.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn progress_starts_busy_deduplicates_and_clears_after_builds() {
    let mut terminal = LiveTerminal::new(Vec::<u8>::new());
    terminal.progress_enabled = Some(true);
    let mut stream =
      crate::monitor::StreamEngine::new(crate::types::EngineConfig::default());
    terminal
      .update_progress(stream.engine().state(), &RenderConfig::default(), false)
      .unwrap();
    terminal
      .update_progress(stream.engine().state(), &RenderConfig::default(), false)
      .unwrap();
    assert_eq!(terminal.writer, b"\x1b]9;4;3;\x1b\\");

    stream.push_at(b"@nix {\"action\":\"start\",\"id\":1,\"level\":3,\"parent\":0,\"text\":\"\",\"type\":104,\"fields\":[]}\n@nix {\"action\":\"result\",\"id\":1,\"type\":105,\"fields\":[1,4,1,0]}\n", 0.0).unwrap();
    terminal
      .update_progress(stream.engine().state(), &RenderConfig::default(), false)
      .unwrap();
    terminal
      .update_progress(stream.engine().state(), &RenderConfig::default(), false)
      .unwrap();
    assert_eq!(count(&terminal.writer, b"\x1b]9;4;1;25\x1b\\"), 1);

    stream.push_at(b"@nix {\"action\":\"result\",\"id\":1,\"type\":105,\"fields\":[4,4,0,0]}\n", 1.0).unwrap();
    terminal
      .update_progress(stream.engine().state(), &RenderConfig::default(), false)
      .unwrap();
    assert!(terminal.writer.ends_with(b"\x1b]9;4;3;\x1b\\"));
    terminal
      .update_progress(stream.engine().state(), &RenderConfig::default(), true)
      .unwrap();
    terminal.finish().unwrap();
    assert_eq!(count(&terminal.writer, b"\x1b]9;4;0;\x1b\\"), 1);
  }

  #[test]
  fn progress_survives_graph_retirement_and_clears_on_drop() {
    let mut bytes = Vec::new();
    let mut terminal = LiveTerminal::new(&mut bytes);
    terminal.progress_enabled = Some(true);
    terminal.retire().unwrap();
    terminal
      .render(&State::new(), &RenderConfig::default(), 0.0, false)
      .unwrap();
    drop(terminal);
    assert_eq!(bytes, b"\x1b]9;4;3;\x1b\\\x1b]9;4;0;\x1b\\");
  }

  #[test]
  fn disabled_progress_emits_nothing() {
    let mut terminal = LiveTerminal::new(Vec::<u8>::new());
    terminal.progress_enabled = Some(false);
    terminal
      .update_progress(&State::new(), &RenderConfig::default(), false)
      .unwrap();
    terminal.finish().unwrap();
    assert_eq!(terminal.writer, b"");
  }

  #[cfg(feature = "cli")]
  #[test]
  fn cancellation_tracks_only_this_renderers_outstanding_report() {
    let outstanding = Arc::new(AtomicBool::new(false));
    let mut terminal = LiveTerminal::new(Vec::<u8>::new());
    terminal.track_progress(Arc::clone(&outstanding));
    terminal.progress_enabled = Some(false);
    terminal
      .update_progress(&State::new(), &RenderConfig::default(), false)
      .unwrap();
    assert!(!outstanding.load(Ordering::SeqCst));
    terminal.progress_enabled = Some(true);
    terminal
      .update_progress(&State::new(), &RenderConfig::default(), false)
      .unwrap();
    assert!(outstanding.load(Ordering::SeqCst));
    terminal.finish().unwrap();
    assert!(!outstanding.load(Ordering::SeqCst));
  }

  #[test]
  fn frame_commit_is_one_synchronized_full_repaint() {
    let config = RenderConfig::default();
    let frame = render_frame(&State::new(), &config, 0.0, 39, 8, true);
    let terminal = LiveTerminal::new(Vec::<u8>::new());
    let bytes = terminal.compose(&[], &frame.ansi_text()).unwrap();
    assert_eq!(count(&bytes, BEGIN_SYNC), 1);
    assert_eq!(count(&bytes, END_SYNC), 1);
    assert!(!bytes.windows(4).any(|window| window == b"\x1b[2J"));
    assert!(!bytes.windows(3).any(|window| window == b"\x1b[r"));
  }

  #[test]
  fn first_build_event_is_present_in_the_first_atomic_commit() {
    let mut stream =
      crate::monitor::StreamEngine::new(crate::types::EngineConfig::default());
    stream
      .push_at(
        b"@nix {\"action\":\"start\",\"id\":10,\"level\":3,\"parent\":0,\"text\":\"building '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo.drv'\",\"type\":105,\"fields\":[\"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo.drv\",\"\",1,1]}\n",
        0.0,
      )
      .unwrap();
    let frame = render_frame(
      stream.engine().state(),
      &RenderConfig::default(),
      1.0,
      79,
      23,
      false,
    );
    assert!(frame.text().starts_with("╭─ Builds\n├─"));
    let terminal = LiveTerminal::new(Vec::<u8>::new());
    let bytes = terminal.compose(&[], &frame.ansi_text()).unwrap();
    assert_eq!(count(&bytes, BEGIN_SYNC), 1);
    assert_eq!(count(&bytes, END_SYNC), 1);
    assert!(bytes.windows(4).any(|window| window == b"demo"));
  }

  #[test]
  fn pending_logs_and_complete_graph_share_one_atomic_commit() {
    let config = RenderConfig::default();
    let frame = render_frame(&State::new(), &config, 0.0, 39, 8, true);
    let mut terminal = LiveTerminal::new(Vec::<u8>::new());
    terminal.initialized = true;
    terminal.graph_height = 2;
    terminal
      .pending
      .extend_from_slice(b"a deliberately very long wrapped log line\n");
    let bytes = terminal
      .compose(&terminal.pending, &frame.ansi_text())
      .unwrap();
    let log = find(&bytes, b"deliberately").unwrap();
    let graph = find(&bytes, b"Finished").unwrap();
    assert!(log < graph);
    assert_eq!(count(&bytes, BEGIN_SYNC), 1);
    assert_eq!(count(&bytes, END_SYNC), 1);
    assert!(!bytes.windows(3).any(|window| window == b"\x1b[r"));
  }

  #[test]
  fn producer_controls_are_buffered_without_retiring_the_graph() {
    let mut terminal = LiveTerminal::new(Vec::<u8>::new());
    terminal.graph_height = 2;
    terminal.initialized = true;
    terminal
      .write_passthrough(b"progress\rreplacement\x1b[K\n")
      .unwrap();
    assert!(!terminal.retired);
    assert_eq!(terminal.pending, b"progress\rreplacement\x1b[K\n");
  }

  #[test]
  fn direct_terminals_are_live_and_multiplexers_fail_closed() {
    assert_eq!(admission_from(false, false), Admission::NotATerminal);
    assert_eq!(admission_from(true, true), Admission::Multiplexer);
    assert_eq!(admission_from(true, false), Admission::Live);
  }

  #[test]
  fn live_rendering_requires_room_for_a_connected_graph() {
    assert!(!live_geometry(MIN_LIVE_COLUMNS - 1, MIN_LIVE_ROWS));
    assert!(!live_geometry(MIN_LIVE_COLUMNS, MIN_LIVE_ROWS - 1));
    assert!(live_geometry(MIN_LIVE_COLUMNS, MIN_LIVE_ROWS));

    let frame = render_frame(
      &State::new(),
      &RenderConfig::default(),
      0.0,
      MIN_LIVE_COLUMNS - 1,
      MIN_LIVE_ROWS - 1,
      false,
    );
    let text = frame.text();
    assert!(text.starts_with("╭─ Builds\n"), "{text}");
    assert!(text.lines().last().unwrap().starts_with("┗━"), "{text}");
  }

  fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
      .windows(needle.len())
      .position(|window| window == needle)
  }

  fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
      .windows(needle.len())
      .filter(|window| *window == needle)
      .count()
  }
}
