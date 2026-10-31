//! Live terminal behavior of the real CLI under a pseudo-terminal.

#![cfg(all(unix, feature = "cli"))]
#![expect(
  clippy::tests_outside_test_module,
  reason = "integration tests are their own crate"
)]
#![expect(clippy::panic, reason = "tests fail by panicking")]

use std::{
  fs::File,
  io::{self, Read, Write},
  os::{
    fd::{FromRawFd, RawFd},
    unix::process::CommandExt,
  },
  process::{Command, Stdio},
  thread,
  time::{Duration, Instant},
};

const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
const END_SYNC: &[u8] = b"\x1b[?2026l";
const CLEAR_LINE: &[u8] = b"\x1b[2K";

#[test]
fn fixture_reaches_a_styled_first_live_frame_through_the_real_cli() {
  let transcript = replay_fixture("unsized-download", false, 80, 24, None);
  assert_eq!(count(&transcript, BEGIN_SYNC), count(&transcript, END_SYNC));
  assert!(count(&transcript, BEGIN_SYNC) > 0);
  assert!(transcript.windows(7).any(|w| w == b"\x1b[1;35m"));
  assert!(transcript.windows(4).any(|w| w == b"demo"));

  let frames = synchronized_transactions(&transcript);
  let initial = frames.first().expect("no initial terminal frame");
  assert!(initial.windows(6).any(|window| window == b"Builds"));
  assert!(initial.windows(6).any(|window| window == b"Status"));
  assert!(initial.windows(7).any(|window| window == b"Elapsed"));
  assert!(!initial.windows(4).any(|window| window == b"demo"));
  let first_graph = frames
    .iter()
    .find(|frame| frame.windows(4).any(|window| window == b"demo"))
    .expect("no atomic graph repaint was committed");
  assert!(first_graph.windows(6).any(|window| window == b"Builds"));
  assert!(first_graph.windows(4).any(|window| window == b"demo"));
  assert!(first_graph.windows(6).any(|window| window == b"Status"));
  assert!(first_graph.windows(7).any(|window| window == b"Elapsed"));

  for (needle, description) in [
    (b"checking".as_slice(), "styled log"),
    (b"configuring flags".as_slice(), "over-width log"),
  ] {
    let log_frame = frames
      .iter()
      .find(|frame| frame.windows(needle.len()).any(|window| window == needle))
      .unwrap_or_else(|| panic!("{description} was not committed"));
    assert!(log_frame.windows(6).any(|window| window == b"Builds"));
    assert!(log_frame.windows(4).any(|window| window == b"demo"));
    assert!(log_frame.windows(6).any(|window| window == b"Status"));
    assert!(log_frame.windows(7).any(|window| window == b"Elapsed"));
  }
  assert!(!transcript.windows(3).any(|window| window == b"\x1b[r"));
}

#[test]
fn shrinking_graph_stays_anchored_to_the_bottom() {
  let transcript = replay_fixture("fetch-to-store", false, 80, 24, None);
  let frames = synchronized_transactions(&transcript);
  // Each repaint clears the previous region one row at a time and then draws
  // logs and the new frame. Drawing fewer rows than were cleared would leave
  // blank rows under the graph. Only the final frame may shrink.
  let repaints = frames
    .iter()
    .filter_map(|frame| {
      let cleared = count(frame, CLEAR_LINE);
      let drawn = find_last(frame, CLEAR_LINE)
        .map(|end| count(&frame[end..], b"\n") + 1)?;
      Some((cleared, drawn))
    })
    .collect::<Vec<(usize, usize)>>();
  assert!(repaints.len() > 2, "too few live repaints to check");
  for (cleared, drawn) in &repaints[..repaints.len() - 1] {
    assert!(
      drawn >= cleared,
      "repaint drew {drawn} rows over {cleared} cleared rows"
    );
  }
}

#[test]
fn multiplexer_gets_a_safe_initial_graph_without_live_controls() {
  let transcript = replay_fixture("unsized-download", true, 80, 24, None);
  assert_eq!(count(&transcript, BEGIN_SYNC), 0);
  assert_eq!(count(&transcript, END_SYNC), 0);
  assert!(!transcript.windows(4).any(|window| window == b"\x1b[2K"));
  assert!(transcript.windows(6).any(|window| window == b"Builds"));
  assert!(transcript.windows(4).any(|window| window == b"demo"));
  assert!(transcript.windows(7).any(|window| window == b"\x1b[1;35m"));
}

#[test]
fn undersized_terminal_falls_back_without_live_control_artifacts() {
  for (columns, rows) in [(19, 8), (80, 7)] {
    let transcript =
      replay_fixture("unsized-download", false, columns, rows, None);
    assert_eq!(count(&transcript, BEGIN_SYNC), 0);
    assert_eq!(count(&transcript, END_SYNC), 0);
    assert!(!transcript.windows(4).any(|window| window == b"\x1b[2K"));
    assert!(transcript.windows(8).any(|window| window == b"checking"));
    assert!(transcript.windows(6).any(|window| window == b"Builds"));
    assert!(transcript.windows(7).any(|window| window == b"\x1b[1;35m"));
  }
}

#[test]
fn osc_progress_supports_auto_detection_and_an_ssh_override() {
  for progress in ["auto", "1"] {
    let transcript =
      replay_fixture("unsized-download", false, 80, 24, Some(progress));
    assert!(find(&transcript, b"\x1b]9;4;3;\x1b\\").is_some());
    assert_eq!(count(&transcript, b"\x1b]9;4;0;\x1b\\"), 1);
  }
}

#[test]
fn osc_progress_opt_out_and_multiplexers_emit_no_reports() {
  for (multiplexer, progress) in [
    (false, "0"),
    (true, "1"),
    (false, "silent"),
    (false, "dumb"),
  ] {
    let transcript =
      replay_fixture("unsized-download", multiplexer, 80, 24, Some(progress));
    assert_eq!(count(&transcript, b"\x1b]9;4;"), 0);
  }
}

#[test]
fn interrupting_a_stream_clears_terminal_progress() {
  for mode in ["interrupt", "terminate", "hangup"] {
    let transcript =
      replay_fixture("unsized-download", false, 80, 24, Some(mode));
    assert!(find(&transcript, b"\x1b]9;4;3;\x1b\\").is_some());
    assert_eq!(count(&transcript, b"\x1b]9;4;0;\x1b\\"), 1);
    assert!(!String::from_utf8_lossy(&transcript).contains("Error:"));
  }
}

#[test]
#[expect(
  clippy::semicolon_inside_block,
  reason = "semicolon_outside_block asks for the opposite"
)]
fn repeated_interrupt_does_not_clear_disabled_progress() {
  use std::os::unix::fs::PermissionsExt;

  let directory = tempfile::tempdir().unwrap();
  let nix = directory.path().join("nix");
  std::fs::write(
    &nix,
    "#!/bin/sh\ntrap '' INT TERM HUP\n: > \"$ROM_TEST_READY\"\nwhile :; do \
     /bin/sleep 1; done\n",
  )
  .unwrap();
  std::fs::set_permissions(&nix, std::fs::Permissions::from_mode(0o755))
    .unwrap();
  let (master, slave) = open_pty(80, 24).unwrap();
  // SAFETY: open_pty returned a fresh, uniquely owned descriptor.
  let slave = unsafe { File::from_raw_fd(slave) };
  let mut command = Command::new(env!("CARGO_BIN_EXE_rom"));
  command
    .args(["--platform", "nix", "--progress", "never", "build", "test"])
    .env("PATH", directory.path())
    .env("XDG_CONFIG_HOME", directory.path())
    .env("XDG_STATE_HOME", directory.path())
    .env("TERM", "dumb")
    .env("ROM_PROGRESS", "0")
    .env("ROM_TEST_READY", directory.path().join("ready"))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::from(slave));
  let hook = || {
    // SAFETY: setsid has no preconditions.
    if unsafe { libc::setsid() } == -1 {
      return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "macos")]
    let request = libc::c_ulong::from(libc::TIOCSCTTY);
    #[cfg(not(target_os = "macos"))]
    let request = libc::TIOCSCTTY;
    // SAFETY: Command installs the PTY as fd 2 before running this hook.
    if unsafe { libc::ioctl(2, request, 0) } == -1 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
  };
  // SAFETY: the hook invokes only async-signal-safe libc operations.
  unsafe { command.pre_exec(hook) };
  let mut child = command.spawn().unwrap();
  drop(command);
  let reader = thread::spawn(move || read_terminal(master));
  let deadline = Instant::now() + Duration::from_secs(10);
  while !directory.path().join("ready").exists() {
    assert!(Instant::now() < deadline, "mock Nix failed to start");
    thread::sleep(Duration::from_millis(10));
  }
  for _ in 0..2 {
    assert_eq!(
      // SAFETY: the child is running and SIGINT has no pointer arguments.
      unsafe { libc::kill(child.id() as i32, libc::SIGINT) },
      0,
      "failed to interrupt rom"
    );
    thread::sleep(Duration::from_millis(100));
  }
  assert_eq!(child.wait().unwrap().code(), Some(130));
  let transcript = reader.join().unwrap();
  assert_eq!(count(&transcript, b"\x1b]9;4;"), 0);
}

#[expect(
  clippy::semicolon_inside_block,
  reason = "semicolon_outside_block asks for the opposite"
)]
fn replay_fixture(
  log: &str,
  multiplexer: bool,
  columns: u16,
  rows: u16,
  progress: Option<&str>,
) -> Vec<u8> {
  let (master, slave) = open_pty(columns, rows).unwrap();
  // SAFETY: `open_pty` returned `slave` as a fresh descriptor nothing else
  // owns.
  let slave = unsafe { File::from_raw_fd(slave) };
  let mut command = Command::new(env!("CARGO_BIN_EXE_rom"));
  command
    .env("TERM", "dumb")
    .env("TERM_PROGRAM", "ghostty")
    .env_remove("STY")
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::from(slave));
  command.env_remove("ROM_PROGRESS");
  if progress == Some("1") {
    command.env("ROM_PROGRESS", "1");
  } else {
    command.args(["--progress", match progress {
      Some("auto") => "auto",
      Some("silent" | "dumb" | "interrupt" | "terminate" | "hangup") => {
        "always"
      },
      _ => "never",
    }]);
  }
  if progress.is_some() && progress != Some("dumb") {
    command.env("TERM", "xterm-256color");
  }
  if progress == Some("silent") {
    command.arg("--silent");
  }
  if progress == Some("1") {
    command.env_remove("TERM_PROGRAM");
  }
  if multiplexer {
    command.env("TMUX", "/tmp/tmux-test,1,0");
  } else {
    command.env_remove("TMUX");
  }
  let hook = || {
    // SAFETY: `setsid` has no preconditions.
    if unsafe { libc::setsid() } == -1 {
      return Err(io::Error::last_os_error());
    }
    // The request is a `c_ulong`, but macOS declares `TIOCSCTTY` as a `u32`.
    #[cfg(target_os = "macos")]
    let request = libc::c_ulong::from(libc::TIOCSCTTY);
    #[cfg(not(target_os = "macos"))]
    let request = libc::TIOCSCTTY;
    // SAFETY: the PTY is installed as fd 2 by Command before the hook
    // executes.
    if unsafe { libc::ioctl(2, request, 0) } == -1 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
  };
  // SAFETY: the hook only invokes async-signal-safe libc operations.
  unsafe { command.pre_exec(hook) };
  let mut child = command.spawn().unwrap();
  // The command retains its stdio configuration; drop it so the parent's copy
  // cannot keep the slave side alive after the child exits.
  drop(command);
  let reader = thread::spawn(move || read_terminal(master));

  let fixture = std::fs::read_to_string(
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures")
      .join(format!("{log}.log")),
  )
  .unwrap();
  let mut stdin = child.stdin.take().unwrap();
  let started = Instant::now();
  for line in fixture.lines() {
    let (at_ms, rest) = line.split_once(' ').unwrap();
    let at = Duration::from_millis(at_ms.parse().unwrap());
    thread::sleep(at.saturating_sub(started.elapsed()));
    if !rest.starts_with("= ") {
      stdin.write_all(format!("{rest}\n").as_bytes()).unwrap();
      stdin.flush().unwrap();
    }
  }
  let signal = match progress {
    Some("interrupt") => Some(libc::SIGINT),
    Some("terminate") => Some(libc::SIGTERM),
    Some("hangup") => Some(libc::SIGHUP),
    _ => None,
  };
  if let Some(signal) = signal {
    // Keep stdin open: cancellation must not depend on the producer exiting.
    assert_eq!(
      // SAFETY: the child PID is valid and SIGINT requires no pointer
      // arguments.
      unsafe { libc::kill(child.id() as i32, signal) },
      0,
      "failed to interrupt rom"
    );
    let status = child.wait().unwrap();
    assert_eq!(
      status.code(),
      Some(128 + signal),
      "interrupted rom exited with {status}"
    );
    drop(stdin);
  } else {
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "rom exited with {status}");
  }
  reader.join().unwrap()
}

fn open_pty(columns: u16, rows: u16) -> io::Result<(RawFd, RawFd)> {
  let mut master = -1;
  let mut slave = -1;
  // macOS takes the window size as `*mut`, so it needs a mutable binding.
  let mut size = libc::winsize {
    ws_row:    rows,
    ws_col:    columns,
    ws_xpixel: 0,
    ws_ypixel: 0,
  };
  // SAFETY: all output pointers are valid and `size` is initialized.
  if unsafe {
    libc::openpty(
      &raw mut master,
      &raw mut slave,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &raw mut size,
    )
  } == -1
  {
    return Err(io::Error::last_os_error());
  }
  Ok((master, slave))
}

fn read_terminal(master: RawFd) -> Vec<u8> {
  // SAFETY: `master` is uniquely owned by this thread.
  let mut terminal = unsafe { File::from_raw_fd(master) };
  let mut transcript = Vec::new();
  let mut buffer = [0_u8; 4096];
  loop {
    match terminal.read(&mut buffer) {
      Ok(0) => break,
      Ok(count) => {
        transcript.extend_from_slice(&buffer[..count]);
      },
      Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
      Err(error) => panic!("PTY read failed: {error}"),
    }
  }
  transcript
}

fn synchronized_transactions(transcript: &[u8]) -> Vec<&[u8]> {
  let mut result = Vec::new();
  let mut remaining = transcript;
  while let Some(begin) = find(remaining, BEGIN_SYNC) {
    let transaction = &remaining[begin..];
    let Some(end) = find(transaction, END_SYNC) else {
      panic!("unterminated synchronized transaction");
    };
    let end = end + END_SYNC.len();
    result.push(&transaction[..end]);
    remaining = &transaction[end..];
  }
  result
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

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack
    .windows(needle.len())
    .rposition(|window| window == needle)
    .map(|index| index + needle.len())
}
