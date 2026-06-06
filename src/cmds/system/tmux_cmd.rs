//! Tmux command wrapper for token-optimized capture-pane and send-keys verification.

use crate::core::runner;
use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::resolved_command;
use anyhow::{Context, Result};

/// Main entry point for the `rtk tmux` subcommand.
pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        return run_passthrough(args, verbose);
    }

    let subcommand = &args[0];
    if subcommand != "capture-pane" && subcommand != "send-keys" {
        return run_passthrough(args, verbose);
    }

    // Extract custom RTK flags and keep the rest for tmux forwarding
    let mut rtk_since_marker = None;
    let mut rtk_marker_required = false;
    let mut rtk_wait = false;
    let mut rtk_timeout = None;
    let mut rtk_prompt = None;
    let mut rtk_poll_interval = None;
    let mut tmux_args = Vec::new();

    tmux_args.push(args[0].clone()); // Keep "capture-pane" or "send-keys"

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--rtk-since-marker" {
            if i + 1 < args.len() {
                rtk_since_marker = Some(args[i + 1].clone());
                i += 2;
            } else {
                return Err(anyhow::anyhow!("Missing value for --rtk-since-marker"));
            }
        } else if arg == "--rtk-marker-required" {
            rtk_marker_required = true;
            i += 1;
        } else if arg == "--rtk-wait" {
            rtk_wait = true;
            i += 1;
        } else if arg == "--rtk-timeout" {
            if i + 1 < args.len() {
                let val = args[i + 1]
                    .parse::<u64>()
                    .context("Invalid value for --rtk-timeout")?;
                rtk_timeout = Some(val);
                i += 2;
            } else {
                return Err(anyhow::anyhow!("Missing value for --rtk-timeout"));
            }
        } else if arg == "--rtk-prompt" {
            if i + 1 < args.len() {
                rtk_prompt = Some(args[i + 1].clone());
                i += 2;
            } else {
                return Err(anyhow::anyhow!("Missing value for --rtk-prompt"));
            }
        } else if arg == "--rtk-poll-interval" {
            if i + 1 < args.len() {
                let val = args[i + 1]
                    .parse::<u64>()
                    .context("Invalid value for --rtk-poll-interval")?;
                rtk_poll_interval = Some(val);
                i += 2;
            } else {
                return Err(anyhow::anyhow!("Missing value for --rtk-poll-interval"));
            }
        } else {
            tmux_args.push(arg.clone());
            i += 1;
        }
    }

    if subcommand == "capture-pane" {
        run_capture_pane(
            &tmux_args,
            rtk_since_marker.as_deref(),
            rtk_marker_required,
            verbose,
        )
    } else {
        run_send_keys(
            &tmux_args,
            rtk_wait,
            rtk_timeout,
            rtk_prompt.as_deref(),
            rtk_poll_interval,
            verbose,
        )
    }
}

/// Passes the command directly through to native tmux with TTY inheritance.
fn run_passthrough(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd_args = Vec::new();
    for arg in args {
        cmd_args.push(std::ffi::OsString::from(arg));
    }
    runner::run_passthrough("tmux", &cmd_args, verbose)
}

/// Handles `rtk tmux capture-pane` execution and filtering.
fn run_capture_pane(
    tmux_args: &[String],
    since_marker: Option<&str>,
    marker_required: bool,
    verbose: u8,
) -> Result<i32> {
    let mut final_args = tmux_args.to_vec();

    // Inject "-S -50" to default to last 50 lines if no scrollback start -S is provided
    let has_s_flag = final_args.iter().any(|arg| arg == "-S");
    if !has_s_flag {
        final_args.insert(1, "-S".to_string());
        final_args.insert(2, "-50".to_string());
    }

    let mut cmd = resolved_command("tmux");
    cmd.args(&final_args);

    let timer = tracking::TimedExecution::start();
    if verbose > 0 {
        eprintln!("Running: tmux {}", final_args.join(" "));
    }

    let result = exec_capture(&mut cmd)?;
    if result.exit_code != 0 {
        eprintln!("stale_target");
        timer.track(
            &format!("tmux {}", final_args.join(" ")),
            "rtk tmux",
            &result.combined(),
            "stale_target",
        );
        return Ok(1);
    }

    let (filtered_output, _marker_found, marker_error) =
        filter_capture_pane(&result.stdout, since_marker, marker_required, verbose)?;

    if marker_error {
        eprintln!("marker_not_found");
        timer.track(
            &format!("tmux {}", final_args.join(" ")),
            "rtk tmux",
            &result.stdout,
            "marker_not_found",
        );
        return Ok(4);
    }

    // Output to stdout if the user passed -p flag
    if final_args.iter().any(|arg| arg == "-p") {
        print!("{}", filtered_output);
        // Ensure a trailing newline if we printed output
        if !filtered_output.is_empty() && !filtered_output.ends_with('\n') {
            println!();
        }
    }

    timer.track(
        &format!("tmux {}", final_args.join(" ")),
        "rtk tmux",
        &result.stdout,
        &filtered_output,
    );

    Ok(0)
}

/// Pure filtering logic for capture-pane output.
pub fn filter_capture_pane(
    raw_output: &str,
    since_marker: Option<&str>,
    marker_required: bool,
    verbose: u8,
) -> Result<(String, bool, bool)> {
    let stripped = crate::core::utils::strip_ansi(raw_output);
    let mut lines: Vec<&str> = stripped.lines().collect();

    // Strip trailing empty/whitespace-only lines
    while let Some(last) = lines.last() {
        if last.trim().is_empty() {
            lines.pop();
        } else {
            break;
        }
    }

    let mut marker_found = false;
    let mut marker_error = false;
    let mut filtered_lines = lines.clone();

    if let Some(marker_str) = since_marker {
        let re = regex::Regex::new(marker_str)
            .map_err(|e| anyhow::anyhow!("Invalid regex pattern for --rtk-since-marker: {}", e))?;
        if let Some(idx) = lines.iter().rposition(|line| re.is_match(line)) {
            filtered_lines = lines[idx + 1..].to_vec();
            marker_found = true;
        } else if marker_required {
            marker_error = true;
        }
    }

    let filtered_output = filtered_lines.join("\n");

    if verbose > 0 {
        eprintln!("rtk tmux capture-pane: Stripped ANSI codes, stripped trailing blank lines.");
        if since_marker.is_some() {
            if marker_found {
                eprintln!(
                    "rtk tmux capture-pane: Marker found; returned {} lines.",
                    filtered_lines.len()
                );
            } else {
                eprintln!(
                    "rtk tmux capture-pane: Marker not found; fallback to default pane output."
                );
            }
        }
    }

    Ok((filtered_output, marker_found, marker_error))
}

/// Handles `rtk tmux send-keys` execution and optional verification.
fn run_send_keys(
    tmux_args: &[String],
    wait: bool,
    timeout: Option<u64>,
    prompt: Option<&str>,
    poll_interval: Option<u64>,
    verbose: u8,
) -> Result<i32> {
    // Zero-overhead fast path when wait is not specified
    if !wait {
        let mut cmd = resolved_command("tmux");
        cmd.args(tmux_args);
        let status = cmd.status().context("Failed to run tmux send-keys")?;
        return Ok(status.code().unwrap_or(1));
    }

    // Extract target to verify existence and check prompt readiness
    let mut target = None;
    let mut j = 1;
    while j < tmux_args.len() {
        if tmux_args[j] == "-t" && j + 1 < tmux_args.len() {
            target = Some(tmux_args[j + 1].clone());
            break;
        } else if tmux_args[j].starts_with("-t") {
            target = Some(tmux_args[j][2..].to_string());
            break;
        }
        j += 1;
    }

    let start_time = std::time::Instant::now();

    // 1. Existence check (stale_target)
    let mut check_cmd = resolved_command("tmux");
    check_cmd.arg("capture-pane");
    if let Some(ref t) = target {
        check_cmd.arg("-t").arg(t);
    }
    check_cmd.arg("-p");

    let check_result = exec_capture(&mut check_cmd)?;
    if check_result.exit_code != 0 {
        eprintln!("stale_target");
        return Ok(1);
    }
    let existence_time = start_time.elapsed().as_millis();

    // 2. Readiness check (not_ready)
    let stripped_check = crate::core::utils::strip_ansi(&check_result.stdout);
    let check_lines: Vec<&str> = stripped_check.lines().collect();
    let mut last_non_empty = "";
    for line in check_lines.iter().rev() {
        if !line.trim().is_empty() {
            last_non_empty = line;
            break;
        }
    }

    let prompt_pattern = prompt.unwrap_or(r"(?m)[❯$%#]\s*$");
    let prompt_re = regex::Regex::new(prompt_pattern)
        .map_err(|e| anyhow::anyhow!("Invalid prompt regex: {}", e))?;

    if !prompt_re.is_match(last_non_empty) {
        eprintln!("not_ready");
        return Ok(2);
    }
    let readiness_time = start_time.elapsed().as_millis() - existence_time;

    // Snapshot pre-send state
    let pre_send_snapshot = stripped_check.trim_end().to_string();

    // 3. Send the keys
    let mut send_cmd = resolved_command("tmux");
    send_cmd.args(tmux_args);
    let send_result = exec_capture(&mut send_cmd)?;
    if send_result.exit_code != 0 {
        eprint!("{}", send_result.stderr);
        return Ok(send_result.exit_code);
    }

    // 4. Verification loop with backoff
    let timeout_duration = std::time::Duration::from_millis(timeout.unwrap_or(2000));
    let loop_start = std::time::Instant::now();
    let mut poll_delay = std::time::Duration::from_millis(50);
    let mut polls = 0;
    let mut verified = false;

    while loop_start.elapsed() < timeout_duration {
        let sleep_dur = if let Some(fixed_interval) = poll_interval {
            std::time::Duration::from_millis(fixed_interval)
        } else {
            poll_delay
        };
        std::thread::sleep(sleep_dur);
        polls += 1;

        if poll_interval.is_none() {
            poll_delay = std::cmp::min(poll_delay * 2, std::time::Duration::from_millis(500));
        }

        let mut poll_cmd = resolved_command("tmux");
        poll_cmd.arg("capture-pane");
        if let Some(ref t) = target {
            poll_cmd.arg("-t").arg(t);
        }
        poll_cmd.arg("-p");

        if let Ok(poll_res) = exec_capture(&mut poll_cmd) {
            if poll_res.exit_code == 0 {
                let current_stripped = crate::core::utils::strip_ansi(&poll_res.stdout);
                let current_trimmed = current_stripped.trim_end().to_string();
                if current_trimmed != pre_send_snapshot {
                    verified = true;
                    break;
                }
            }
        }
    }

    if !verified {
        eprintln!("timeout");
        return Ok(3);
    }

    let total_verify_time = loop_start.elapsed().as_millis();

    if verbose > 0 {
        eprintln!(
            "rtk tmux send-keys: Pane existence check passed in {}ms",
            existence_time
        );
        eprintln!(
            "rtk tmux send-keys: Prompt readiness check passed in {}ms",
            readiness_time
        );
        eprintln!(
            "rtk tmux send-keys: Verification succeeded in {}ms after {} polls",
            total_verify_time, polls
        );
    }

    let timer = tracking::TimedExecution::start();
    timer.track(&format!("tmux {}", tmux_args.join(" ")), "rtk tmux", "", "");

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_capture_pane_basic() {
        let raw = "line 1\nline 2\nline 3\n\n  \n";
        let (filtered, marker_found, marker_error) =
            filter_capture_pane(raw, None, false, 0).unwrap();
        assert_eq!(filtered, "line 1\nline 2\nline 3");
        assert!(!marker_found);
        assert!(!marker_error);
    }

    #[test]
    fn test_filter_capture_pane_ansi() {
        let raw = "\u{1b}[31mred line\u{1b}[0m\nnormal line\n";
        let (filtered, _, _) = filter_capture_pane(raw, None, false, 0).unwrap();
        assert_eq!(filtered, "red line\nnormal line");
    }

    #[test]
    fn test_filter_capture_pane_since_marker() {
        let raw = "line 1\n❯ /leader analysis\nresult line 1\nresult line 2\n";
        let (filtered, marker_found, marker_error) =
            filter_capture_pane(raw, Some(r"❯ /leader"), false, 0).unwrap();
        assert_eq!(filtered, "result line 1\nresult line 2");
        assert!(marker_found);
        assert!(!marker_error);
    }

    #[test]
    fn test_filter_capture_pane_marker_not_found_fallback() {
        let raw = "line 1\nline 2\nline 3\n";
        let (filtered, marker_found, marker_error) =
            filter_capture_pane(raw, Some(r"nonexistent"), false, 0).unwrap();
        assert_eq!(filtered, "line 1\nline 2\nline 3");
        assert!(!marker_found);
        assert!(!marker_error);
    }

    #[test]
    fn test_filter_capture_pane_marker_required_error() {
        let raw = "line 1\nline 2\nline 3\n";
        let (_filtered, marker_found, marker_error) =
            filter_capture_pane(raw, Some(r"nonexistent"), true, 0).unwrap();
        assert!(!marker_found);
        assert!(marker_error);
    }
}

