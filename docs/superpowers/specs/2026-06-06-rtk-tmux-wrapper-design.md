# Spec: `rtk tmux` wrapper for `capture-pane` and `send-keys`

- **Date:** 2026-06-06
- **Status:** Approved (with revisions)
- **Author:** Antigravity

---

## 1. Problem Statement

In multi-agent environments (e.g. Claude Code worker pools, CTO-worker architectures), agents orchestrate tasks by communicating across `tmux` panes. The two commands dominating this workflow are:
1. `tmux capture-pane -t <target> -p` — used to read output from another pane (frequently polling build/test logs).
2. `tmux send-keys -t <target> ...` — used to dispatch commands to another pane.

This workflow has two major inefficiencies:
- **Token Waste:** `capture-pane` returns a large amount of stale text (often 200+ lines padded to pane height) and ANSI escape sequences when the LLM only needs the most recent lines or text since a specific command/marker.
- **Reliability / Silencing:** A standard `send-keys` succeeds immediately at the `tmux` level but does not guarantee the target pane's shell or agent actually consumed it. Callers have no way to verify prompt readiness or command execution without manually reading pane content, which burns context.

---

## 2. Proposed Architecture

We will add a new `rtk tmux` subcommand that intercepts arguments, handles standard `tmux` operations, and applies token reduction filters and verification loops.

### 2.1 CLI Interface & Argument Separation

To avoid collisions with current or future native `tmux` flags, all custom `rtk` parameters are prefixed with `--rtk-`. Unrecognized subcommands (e.g. `list-sessions`, `kill-session`) bypass all interception and are passed through directly to the native `tmux` binary.

The new subcommand is registered in `src/main.rs` as:

```rust
    /// Tmux wrapper for capture-pane and send-keys
    Tmux {
        /// Arguments passed to tmux
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
```

Inside `src/cmds/system/tmux_cmd.rs`, `args` is parsed manually to extract custom `rtk` flags.

#### Intercepted Custom Flags:
* `--rtk-since-marker <regex>` (for `capture-pane`): Filters output to only return lines printed after the last occurrence of the pattern matching `<regex>`.
* `--rtk-marker-required` (for `capture-pane`): Errors out if the specified marker is not found (rather than falling back to default behavior).
* `--rtk-wait` (for `send-keys`): Enables delivery-receipt checking.
* `--rtk-timeout <ms>` (for `send-keys`): Sets the timeout duration in milliseconds (default: `2000`).
* `--rtk-prompt <regex>` (for `send-keys`): Specifies a custom regex pattern to identify shell prompts (default: `r"(?m)[❯$%#]\s*$"`).
* `--rtk-poll-interval <ms>` (for `send-keys`): Specifies a fixed polling interval (disables backoff).

---

## 3. Detailed Component Designs

### 3.1 `rtk tmux capture-pane`

Spawns native `tmux capture-pane` after applying the following preprocessing:

1. **Automatic Line Limit:** If no `-S` flag is supplied by the user, `rtk` automatically injects `-S` and `-50` to limit scrollback capture to the last 50 lines by default. If `-S` is present (e.g., `-S -200`), the user's value is preserved.
2. **Path Resolution:** Resolves the `tmux` binary path using `crate::core::utils::resolved_command("tmux")` (assuming Tmux v1.8+).
3. **ANSI Code Stripping:** Removes all ANSI escape codes and styling sequences using `crate::core::utils::strip_ansi` to prevent token waste.
4. **Trailing Blank Stripping:** `tmux` pads output to pane height with blank lines. `rtk` trims all trailing empty/whitespace-only lines.
5. **Marker Filtering:** If `--rtk-since-marker <regex>` is provided:
   - Compile the regex and search lines from bottom to top.
   - If a match is found at index `idx`, discard all lines from `0` to `idx`, and return only lines from `idx + 1` to the end.
   - If no match is found:
     - If `--rtk-marker-required` is specified, write `marker_not_found` to `stderr` and exit with `4`.
     - Otherwise, write a warning to `stderr` and fall back to returning the default lines (e.g. last 50 lines).
6. **Error / Stale Target Handling:** If native `capture-pane` fails (e.g. exit code != 0, session/pane not found), write `stale_target` to `stderr` and exit with `1`.

---

### 3.2 `rtk tmux send-keys`

Spawns native `tmux send-keys` after performing verification:

1. **Wait Opt-In Execution:**
   - If `--rtk-wait` is **not** specified, bypass all existence checks, readiness checks, and snapshots. Spawns `tmux send-keys` directly to minimize latency overhead.
   - If `--rtk-wait` is specified, perform the checks described below.
2. **Existence Check (`stale_target`):** Runs `tmux capture-pane -t <target> -p`. If it fails (pane or session does not exist), writes `stale_target` to `stderr` and exits with `1`.
3. **Readiness Check (`not_ready`):** Captures the current pane text. Strips trailing whitespace, gets the last non-empty line, and matches it against the prompt regex. If it does not match, writes `not_ready` to `stderr` and exits with `2`.
   * **Note on Race Conditions:** This check is best-effort and non-atomic; the target pane could become busy in the small window before keys are dispatched.
4. **Pasting and Verifying Change (`timeout`):**
   - Saves a snapshot of the pre-send pane text (stripped of trailing whitespace and ANSI codes).
   - Executes native `tmux send-keys <args>`.
   - Polls `tmux capture-pane -t <target> -p` at regular intervals.
     - **Backoff Strategy:** Start polling at 50ms, double the delay each time (50ms → 100ms → 200ms → 400ms), and cap the delay at 500ms. If `--rtk-poll-interval` is specified, use that fixed interval instead.
     - If the text content changes from the pre-send snapshot, exit `0` (suppressing all standard success outputs).
     - If the content does not change within the specified timeout, write `timeout` to `stderr` and exit with `3`.
5. **Successful Suppression:** If the keys are successfully sent (and verified if `--rtk-wait` is specified), `rtk` suppresses all output and exits `0`.

---

## 4. Diagnostics & Error Matrix

On failure, `rtk tmux` commands exit with one of the following diagnostics:

| Code | Diagnostic | Command | Scenario |
|------|------------|---------|----------|
| `0`  | *None* | *Both* | Command succeeded. |
| `1`  | `stale_target` | `capture-pane` | Target pane or session does not exist. |
| `1`  | `stale_target` | `send-keys --rtk-wait` | Target pane or session does not exist (only checked with `--rtk-wait`). |
| `2`  | `not_ready` | `send-keys` | Target pane is active/busy (does not show prompt). |
| `3`  | `timeout` | `send-keys` | Keys were sent but the pane content did not change within timeout. |
| `4`  | `marker_not_found` | `capture-pane` | `--rtk-marker-required` was passed, but the marker regex was not found. |

All error diagnostics are written to `stderr` to keep `stdout` completely clean for orchestration pipelines.

---

## 5. Integration and Hooks

We will register rewrite patterns in `src/discover/rules.rs`:

```rust
    RtkRule {
        pattern: r"^tmux\s+(capture-pane|send-keys)",
        rtk_cmd: "rtk tmux",
        rewrite_prefixes: &["tmux"],
        category: "System",
        savings_pct: 75.0,
        subcmd_savings: &[
            ("capture-pane", 85.0),
            ("send-keys", 95.0),
        ],
        subcmd_status: &[],
    },
```

---

## 6. Verbose Logging Integration

When the global `--verbose` flag is passed (e.g. `rtk -v tmux ...`), `rtk` outputs diagnostic details to `stderr`:
- For `capture-pane`: `"Stripped N trailing blank lines"`, `"Stripped ANSI escape codes"`, `"Marker found at line M (returned K lines)"`, or `"Fallback to default pane output"`.
- For `send-keys`: `"Pane existence check passed in Xms"`, `"Prompt readiness check passed in Yms"`, `"Verification succeeded in Zms after P polls"`, or details about failures.

---

## 7. Testing Strategy

1. **Unit Tests (Pure Functions):**
   - The filtering logic is extracted into a pure function:
     `fn filter_capture_pane(output: &str, line_limit: Option<usize>, marker: Option<&Regex>, strip_ansi: bool) -> String`
   - Test cases:
     - ANSI stripping correctness.
     - Trailing blank line removal.
     - Since-marker match and slice logic (both single-line and multiline patterns).
     - Default line limits (50 lines).
2. **Integration Tests (Mocked / Local Tmux):**
   - Verify argument parsing (extracting `--rtk-` options and separating them from forwarded options).
   - If a local tmux daemon is detected during testing, integration tests can launch isolated sessions on a custom socket (`tmux -L rtk-test-socket`) to perform real capture-pane/send-keys roundtrips.
