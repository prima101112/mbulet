# Milestone 2: Implementation Plan for Next Phase

## Executive Summary

This milestone focuses on performance optimization, rich TUI stability, enhanced session metadata, and configuration management. The work is organized into 4 phases with clear acceptance criteria and rollout order.

**Timeline Estimate:** 4-6 weeks (assuming part-time development)

**Risk Level:** Medium - Some work touches critical rendering/attach paths

---

## Phase 1: Paste Performance Improvements

**Goal:** Optimize large paste operations and reduce input latency.

**Current State:**
- Each key event triggers separate `ClientMsg::Input` message
- No input batching
- Render loop runs at fixed rate regardless of activity
- Large pastes (1000+ lines) cause noticeable lag

### Milestones

#### 1.1 Input Batching System
**Priority:** High  
**Risk:** Low  
**Estimated Effort:** 3-4 days

**Implementation Tasks:**
- [ ] Add `input_buffer: Vec<u8>` to client App state
- [ ] Implement 10ms debounce timer for input aggregation
- [ ] Batch consecutive key events before sending `ClientMsg::Input`
- [ ] Add flush mechanism for immediate commands (Enter, Ctrl+C)
- [ ] Preserve input ordering guarantees

**Acceptance Criteria:**
- [ ] 1000-line paste completes in <2 seconds (was ~5 seconds)
- [ ] No perceptible lag for normal typing (latency <50ms)
- [ ] Ctrl+C always interrupts immediately (no batching delay)
- [ ] Input order preserved exactly (verified via echo test)

**Files to Modify:**
- `src/client.rs`: Event loop, input batching logic
- Possibly `src/protocol.rs`: Batch size limits

**Testing Strategy:**
```bash
# Paste performance test
time (cat large_file.txt | pbcopy && mbulet_paste_test)

# Latency test
measure_input_latency.sh  # Ctrl+C interrupt timing
```

**Rollback Plan:** Revert to per-event sends if ordering issues detected

---

#### 1.2 Adaptive Render Rate
**Priority:** Medium  
**Risk:** Low  
**Estimated Effort:** 2-3 days

**Implementation Tasks:**
- [ ] Track `last_pty_output` timestamp per session
- [ ] Implement tiered render rates:
  - Active output: 60 FPS (16ms)
  - Idle (no output for 100ms): 15 FPS (66ms)
  - Background (no output for 1s): 2 FPS (500ms)
- [ ] Add `dirty` flag to skip redundant renders
- [ ] Preserve immediate rendering for user input

**Acceptance Criteria:**
- [ ] CPU usage drops to <1% when idle (was ~3-5%)
- [ ] Active sessions render at 60 FPS during output
- [ ] No stuttering during rapid session switches
- [ ] Keystroke echo appears immediately (<20ms)

**Files to Modify:**
- `src/client.rs`: Main event loop timing

**Testing Strategy:**
```bash
# CPU usage test
top -pid $(pgrep mbulet) -stats cpu,mem -l 60

# Render smoothness test
yes | head -1000  # Rapid output, verify smooth scroll
```

**Rollback Plan:** Return to fixed 30 FPS if frame drops detected

---

#### 1.3 PTY Output Throttling (Daemon-Side)
**Priority:** Low  
**Risk:** Medium - Affects all clients  
**Estimated Effort:** 2 days

**Implementation Tasks:**
- [ ] Add 1MB/s rate limit to PTY reader thread
- [ ] Implement token bucket algorithm for burst allowance
- [ ] Add `throttled` flag to `PtyOutput` message for UI indication
- [ ] Preserve CWD updates (OSC 7) during throttling

**Acceptance Criteria:**
- [ ] `cat /dev/urandom | base64` doesn't freeze client
- [ ] CWD tracking works during high-throughput commands
- [ ] Throttle indicator appears in UI when active
- [ ] No data loss (buffering handles bursts)

**Files to Modify:**
- `src/session.rs`: PTY reader thread
- `src/protocol.rs`: `PtyOutput` message (add `throttled` field)
- `src/client.rs`: Throttle indicator rendering

**Testing Strategy:**
```bash
# Throughput test
dd if=/dev/urandom bs=1M count=100 | base64
# Verify: no freeze, UI responsive, throttle indicator shows
```

**Rollback Plan:** Remove rate limit if data corruption detected

---

### Phase 1 Risks & Mitigations

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|------------|
| Input reordering during batching | Low | High | Extensive order-preservation tests, flush on control chars |
| Render jank during adaptive rate | Medium | Medium | Keep minimum 15 FPS, dirty flag for skip logic |
| Throttling breaks app output | Low | High | Whitelist OSC sequences, large burst allowance |

---

## Phase 2: Rich/Fullscreen TUI Reattach & Theme Stability

**Goal:** Improve experience when detaching/reattaching to sessions running fullscreen TUI apps (vim, htop, less).

**Current State:**
- Alternate screen apps may lose state on reattach
- vt100 parser doesn't persist alternate buffer
- No theme/color persistence across client restarts

### Milestones

#### 2.1 Alternate Screen Buffer Support
**Priority:** High  
**Risk:** High - Core terminal emulation change  
**Estimated Effort:** 5-7 days

**Implementation Tasks:**
- [ ] Research vt100 crate alternate screen capabilities
- [ ] Add alternate buffer tracking to `Session` parser
- [ ] Serialize both primary and alternate buffers on detach
- [ ] Restore both buffers on reattach (via parser state)
- [ ] Test with vim, nvim, less, htop, tmux

**Acceptance Criteria:**
- [ ] Vim session survives detach/reattach with file visible
- [ ] Less pager preserves scroll position and content
- [ ] Htop continues updating after reattach
- [ ] `tput smcup; echo test; sleep 5; tput rmcup` works correctly

**Files to Modify:**
- `src/session.rs`: Parser state serialization
- `src/daemon.rs`: Buffer preservation logic
- `src/client.rs`: Alternate screen rendering

**Testing Strategy:**
```bash
# Vim test
vim large_file.txt
# Detach (Ctrl+B d), reattach, verify file visible and editable

# Less test
less /var/log/system.log
# Scroll down, detach, reattach, verify position preserved
```

**Rollback Plan:** Disable alternate screen if corruption detected, fall back to primary buffer only

---

#### 2.2 Parser State Persistence
**Priority:** Medium  
**Risk:** Medium  
**Estimated Effort:** 3-4 days

**Implementation Tasks:**
- [ ] Implement `parser.save_state()` → serializable format
- [ ] Store parser state in `Session` on detach
- [ ] Add `parser.restore_state(data)` on attach
- [ ] Preserve cursor position, styles, scrollback
- [ ] Handle parser version mismatches gracefully

**Acceptance Criteria:**
- [ ] Colored output (ANSI codes) preserved across reattach
- [ ] Cursor position restored exactly
- [ ] Scrollback buffer intact (up to ring buffer limit)
- [ ] No visible "flash" or redraw on reattach

**Files to Modify:**
- `src/session.rs`: State serialization hooks
- `src/daemon.rs`: State storage
- Possibly upgrade `vt100` crate if serialization unavailable

**Testing Strategy:**
```bash
# Color preservation test
ls --color=always
# Detach, reattach, verify colors intact

# Cursor position test
echo "line 1"; echo "line 2"; printf "partial"
# Detach, reattach, verify cursor after "partial"
```

**Rollback Plan:** Skip state restoration if deserialization fails, fallback to clean attach

---

#### 2.3 Theme Configuration & Persistence
**Priority:** Low  
**Risk:** Low  
**Estimated Effort:** 2-3 days

**Implementation Tasks:**
- [ ] Define `Theme` struct with Color fields for UI elements
- [ ] Add `~/.config/mbulet/theme.toml` config file
- [ ] Implement TOML parsing with `serde` and `toml` crate
- [ ] Apply theme colors to sidebar, bars, borders, highlights
- [ ] Add `--theme` CLI flag for runtime override

**Acceptance Criteria:**
- [ ] Config file changes apply on next client start
- [ ] Invalid config falls back to default theme gracefully
- [ ] All UI elements respect theme colors
- [ ] Example themes provided (dark, light, solarized)

**Files to Modify:**
- `src/client.rs`: Theme application, config loading
- `Cargo.toml`: Add `toml` dependency
- New file: `src/theme.rs` for Theme struct and loading

**Example Config:**
```toml
# ~/.config/mbulet/theme.toml
[colors]
sidebar_bg = { r = 18, g = 18, b = 28 }
bar_bg = { r = 18, g = 18, b = 28 }
highlight = { r = 100, g = 150, b = 255 }
border = { r = 80, g = 80, b = 100 }
```

**Testing Strategy:**
- Manual verification with sample configs
- Invalid config test (malformed TOML)

**Rollback Plan:** N/A (additive feature, no breaking changes)

---

### Phase 2 Risks & Mitigations

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|------------|
| vt100 crate lacks alternate screen API | Medium | High | Fallback to forked crate or alternate parser library |
| Parser state corruption on restore | Low | High | Version tagging, checksum validation, graceful fallback |
| Theme config breaks client startup | Low | Medium | Strict schema validation, default fallback |

---

## Phase 3: Rich Session Details

**Goal:** Display session metadata (name, CWD, git branch, active app/agent) in terminal pane.

**Current State:**
- Session name only visible in sidebar
- CWD tracked but not displayed in terminal view
- No git branch detection
- No active app/agent detection

### Milestones

#### 3.1 Session Metadata Display Bar
**Priority:** High  
**Risk:** Low  
**Estimated Effort:** 2-3 days

**Implementation Tasks:**
- [ ] Add metadata bar above terminal pane (1 row height)
- [ ] Display: `[session-name] ~/path/to/dir (git:branch)`
- [ ] Update layout constraints to allocate space
- [ ] Adjust `pane_size()` calculation for new bar
- [ ] Add toggle keybinding to hide/show bar (Ctrl+B m)

**Acceptance Criteria:**
- [ ] Metadata bar visible by default above terminal
- [ ] Session name displayed and updated on rename
- [ ] CWD displayed and updated on OSC 7 changes
- [ ] Git branch displayed (see 3.2)
- [ ] Bar can be toggled off for full terminal space

**Files to Modify:**
- `src/client.rs`: UI layout, metadata rendering, pane_size()

**Testing Strategy:**
```bash
# Verify metadata updates
cd /tmp && git init test && cd test
# Observe: CWD and git branch appear in bar

# Rename session
# Verify: Name updates in metadata bar
```

**Rollback Plan:** Hide bar by default if layout issues detected

---

#### 3.2 Git Branch Detection
**Priority:** Medium  
**Risk:** Low  
**Estimated Effort:** 2-3 days

**Implementation Tasks:**
- [ ] Add `git_branch: Option<String>` to `SessionInfo` and `ClientSession`
- [ ] Run `git rev-parse --abbrev-ref HEAD` in session CWD on change
- [ ] Cache branch result, refresh on CWD change
- [ ] Handle non-git directories gracefully (display nothing)
- [ ] Add protocol message `DaemonMsg::GitBranchUpdate { id, branch }`

**Acceptance Criteria:**
- [ ] Git branch appears in metadata bar for git repos
- [ ] Non-git directories show no branch (no errors logged)
- [ ] Branch updates when switching branches (`git checkout`)
- [ ] Detached HEAD shows commit hash (first 7 chars)

**Files to Modify:**
- `src/session.rs`: Branch detection on CWD change
- `src/protocol.rs`: `GitBranchUpdate` message, `SessionInfo` field
- `src/daemon.rs`: Send branch updates
- `src/client.rs`: Render branch in metadata bar

**Testing Strategy:**
```bash
# Branch detection test
cd ~/code/myrepo && git checkout main
# Verify: "git:main" appears

git checkout feature-branch
# Verify: "git:feature-branch" appears

git checkout HEAD~1  # Detached HEAD
# Verify: "git:abc1234" appears
```

**Rollback Plan:** Disable branch detection if performance issues (cache aggressively)

---

#### 3.3 Active App/Agent Detection
**Priority:** Low  
**Risk:** Medium - Process inspection may be fragile  
**Estimated Effort:** 4-5 days

**Implementation Tasks:**
- [ ] Query PTY foreground process group (`tcgetpgrp()`)
- [ ] Read `/proc/{pid}/cmdline` to get process name
- [ ] Detect known apps: `vim`, `nvim`, `less`, `htop`, `codex`, `opencode`, `claude`
- [ ] Add icon/label to metadata bar for active app
- [ ] Poll every 1s (when attached) to catch app changes
- [ ] Handle macOS compatibility (no `/proc`, use `ps` instead)

**Acceptance Criteria:**
- [ ] "vim" appears in metadata bar when editing
- [ ] "codex" appears when AI agent active
- [ ] App label updates within 2s of app start/stop
- [ ] Works on both Linux and macOS
- [ ] No polling when session detached (CPU efficiency)

**Files to Modify:**
- `src/session.rs`: Foreground process detection, platform-specific code
- `src/protocol.rs`: `ActiveAppUpdate { id, app }` message
- `src/daemon.rs`: Periodic app polling
- `src/client.rs`: Render app in metadata bar

**Platform-Specific:**
```rust
#[cfg(target_os = "linux")]
fn get_fg_process(pty_fd: RawFd) -> Option<String> {
    // Use tcgetpgrp() + /proc/{pid}/cmdline
}

#[cfg(target_os = "macos")]
fn get_fg_process(pty_fd: RawFd) -> Option<String> {
    // Use tcgetpgrp() + ps -p {pid} -o comm=
}
```

**Testing Strategy:**
```bash
# Linux test
vim test.txt
# Verify: "vim" appears in metadata bar

# macOS test
nvim test.txt
# Verify: "nvim" appears

# Agent test (if available)
codex "test prompt"
# Verify: "codex" appears
```

**Rollback Plan:** Disable app detection on unsupported platforms, make opt-in via config

---

### Phase 3 Risks & Mitigations

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|------------|
| Git branch detection slows CWD updates | Low | Medium | Async execution, aggressive caching |
| Process inspection fails on macOS | Medium | Low | Platform-specific fallbacks, feature flag |
| Metadata bar breaks pane sizing | Low | Medium | Extensive resize testing, runtime validation |

---

## Phase 4: Config Knobs & Validation Strategy

**Goal:** Externalize hard-coded settings and implement robust configuration validation.

**Current State:**
- All settings hard-coded (prefix key, sidebar width, colors)
- No config file
- No validation for user input (session names, reorder indices)

### Milestones

#### 4.1 Configuration File Structure
**Priority:** High  
**Risk:** Low  
**Estimated Effort:** 3-4 days

**Implementation Tasks:**
- [ ] Define `~/.config/mbulet/config.toml` schema
- [ ] Add `config` crate or use `toml` + `serde`
- [ ] Implement config loading with defaults
- [ ] Add `--config` CLI flag for custom config path
- [ ] Document all config options in example file

**Config Schema:**
```toml
# ~/.config/mbulet/config.toml

[client]
prefix_key = "b"
prefix_modifiers = ["ctrl"]
sidebar_width = 22
render_fps_idle = 15
render_fps_active = 60
show_metadata_bar = true

[daemon]
max_sessions = 100
pty_buffer_size = 65536
throttle_mbps = 1.0

[session]
default_shell = "zsh"
enable_osc7 = true
enable_git_branch = true
enable_active_app = true
app_poll_interval_ms = 1000

[theme]
sidebar_bg = { r = 18, g = 18, b = 28 }
bar_bg = { r = 18, g = 18, b = 28 }
highlight = { r = 100, g = 150, b = 255 }
border = { r = 80, g = 80, b = 100 }
```

**Acceptance Criteria:**
- [ ] Config file loaded on client/daemon startup
- [ ] Missing config uses sensible defaults
- [ ] Invalid config prints helpful error and exits gracefully
- [ ] `--config` flag overrides default path
- [ ] Example config shipped with repo

**Files to Modify:**
- `src/main.rs`: Config loading, CLI flag parsing
- New file: `src/config.rs` for Config struct and validation
- `Cargo.toml`: Add `toml`, `serde`, `clap` dependencies
- `src/client.rs`: Use config values instead of constants
- `src/daemon.rs`: Use config values

**Testing Strategy:**
```bash
# Valid config test
mbulet --config examples/config.toml
# Verify: Settings applied

# Invalid config test
echo "invalid toml {" > /tmp/bad.toml
mbulet --config /tmp/bad.toml
# Verify: Clear error message, no crash

# Missing config test
mbulet
# Verify: Defaults used, no error
```

**Rollback Plan:** N/A (additive feature)

---

#### 4.2 Input Validation Layer
**Priority:** Medium  
**Risk:** Low  
**Estimated Effort:** 2 days

**Implementation Tasks:**
- [ ] Add session name validation (max length, allowed chars)
- [ ] Add reorder index bounds checking (redundant safety)
- [ ] Add PTY dimension validation (min 1x1, max 1000x1000)
- [ ] Add startup command validation (no null bytes, length limit)
- [ ] Return descriptive errors via `DaemonMsg::Error`

**Validation Rules:**
```rust
// Session names
- Length: 1-64 chars
- Allowed: alphanumeric, dash, underscore, space
- Forbidden: /, \, null bytes

// Reorder indices
- Must be < sessions.len()
- Cannot move to current position (no-op)

// PTY dimensions
- cols: 1-1000
- rows: 1-1000

// Startup commands
- Total length: <10KB
- No null bytes
- Individual command: <1KB
```

**Acceptance Criteria:**
- [ ] Invalid session name rejected with clear error
- [ ] Out-of-bounds reorder index rejected
- [ ] Zero-dimension resize rejected
- [ ] Malformed startup commands rejected
- [ ] All rejections return `DaemonMsg::Error { msg }`

**Files to Modify:**
- `src/daemon.rs`: Validation functions, error returns
- `src/client.rs`: Display validation errors to user

**Testing Strategy:**
```bash
# Session name validation test
# Try: empty name, 100-char name, name with slashes
# Verify: Rejected with helpful error

# Reorder validation test
# Try: reorder to index 999 (out of bounds)
# Verify: Rejected

# Dimension validation test
# Resize to 0x0
# Verify: Rejected
```

**Rollback Plan:** Remove validation if false positives detected, log instead of reject

---

#### 4.3 Config Hot-Reload (Optional Enhancement)
**Priority:** Low  
**Risk:** Medium  
**Estimated Effort:** 3 days

**Implementation Tasks:**
- [ ] Watch `~/.config/mbulet/config.toml` for changes (using `notify` crate)
- [ ] Reload config on file modification
- [ ] Apply non-destructive changes immediately (colors, render rate)
- [ ] Defer destructive changes (prefix key, sidebar width) to next restart
- [ ] Add `Ctrl+B R` keybinding to force config reload

**Acceptance Criteria:**
- [ ] Theme changes apply within 1s of file save
- [ ] Render rate changes apply immediately
- [ ] Prefix key changes require restart (with warning)
- [ ] Manual reload works via keybinding

**Files to Modify:**
- `src/client.rs`: Config watcher thread, reload logic
- `Cargo.toml`: Add `notify` dependency

**Testing Strategy:**
```bash
# Hot-reload test
vim ~/.config/mbulet/config.toml
# Change sidebar_bg color, save
# Verify: Color updates within 1s (no restart)

# Destructive change test
# Change prefix_key to 'a', save
# Verify: Warning displayed, restart required
```

**Rollback Plan:** Make hot-reload opt-in via config flag if instability detected

---

### Phase 4 Risks & Mitigations

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|------------|
| Config parsing breaks existing behavior | Low | High | Extensive default testing, backward compatibility |
| Hot-reload causes race conditions | Medium | Medium | Mutex-protected config struct, atomic swaps |
| Validation too strict, rejects valid input | Low | Medium | Conservative rules, user feedback loop |

---

## Cross-Phase Considerations

### Backward Compatibility
- Protocol changes (new messages) must be additive
- Old clients should gracefully ignore unknown messages
- Add protocol version field for future compatibility checks

### Performance Regression Prevention
- Benchmark before/after for each phase
- CPU/memory profiling for daemon and client
- Automated performance tests (optional)

### Documentation Updates
Each phase should include:
- [ ] Update README with new features
- [ ] Add config examples
- [ ] Document new keybindings
- [ ] Update manual test guides

---

## Rollout Order & Dependencies

### Recommended Sequence

1. **Phase 4.1 (Config File)** - Foundational, enables other phases
2. **Phase 1.1 (Input Batching)** - Independent, high impact
3. **Phase 1.2 (Adaptive Render)** - Builds on batching
4. **Phase 3.1 (Metadata Bar)** - UI foundation for session details
5. **Phase 3.2 (Git Branch)** - Requires metadata bar
6. **Phase 2.1 (Alternate Screen)** - High risk, needs testing time
7. **Phase 2.2 (Parser Persistence)** - Builds on alternate screen
8. **Phase 1.3 (PTY Throttling)** - Optional optimization
9. **Phase 3.3 (Active App)** - Optional enhancement
10. **Phase 2.3 (Theme Persistence)** - Polish, requires config file
11. **Phase 4.2 (Input Validation)** - Hardening, low priority
12. **Phase 4.3 (Hot-Reload)** - Nice-to-have, optional

### Dependency Graph
```
Config File (4.1) ──┬──> Theme Persistence (2.3)
                    ├──> Hot-Reload (4.3)
                    └──> Input Batching (1.1) ──> Adaptive Render (1.2)

Metadata Bar (3.1) ──┬──> Git Branch (3.2)
                     └──> Active App (3.3)

Alternate Screen (2.1) ──> Parser Persistence (2.2)

(Independent)
- PTY Throttling (1.3)
- Input Validation (4.2)
```

---

## Success Metrics

### Performance
- [ ] Paste speed: 1000 lines in <2s (was ~5s)
- [ ] Idle CPU usage: <1% (was 3-5%)
- [ ] Input latency: <50ms p99 (was ~100ms)
- [ ] Render smoothness: 60 FPS during active output

### Stability
- [ ] No crashes during 100+ session switch cycles
- [ ] Fullscreen apps survive detach/reattach (vim, less, htop)
- [ ] Config file errors never crash daemon
- [ ] Invalid protocol messages logged, not fatal

### User Experience
- [ ] Session metadata visible at all times
- [ ] Theme customization without recompile
- [ ] Git branch visible for all git repositories
- [ ] Active app indicator for common tools

### Code Quality
- [ ] All new code reviewed and tested
- [ ] Test coverage for config validation
- [ ] Documentation updated for all features
- [ ] No new compiler warnings

---

## Testing Strategy

### Unit Tests (To Be Added)
```rust
#[cfg(test)]
mod tests {
    // Phase 1
    #[test] fn test_input_batching_preserves_order() { }
    #[test] fn test_adaptive_render_rate_transitions() { }
    
    // Phase 2
    #[test] fn test_alternate_screen_save_restore() { }
    #[test] fn test_parser_state_serialization() { }
    
    // Phase 3
    #[test] fn test_git_branch_detection() { }
    #[test] fn test_active_app_detection() { }
    
    // Phase 4
    #[test] fn test_config_validation() { }
    #[test] fn test_session_name_validation() { }
}
```

### Integration Tests (Manual)
Each phase ships with:
- Manual test procedure document
- Example test scripts
- Expected vs. actual behavior checklist

### Regression Tests
Before each phase:
- [ ] Run Milestone 1 manual test suite
- [ ] Verify all existing keybindings work
- [ ] Check session switching, renaming, reordering
- [ ] Confirm no content duplication

---

## Rollback Procedures

### Per-Phase Rollback
Each phase should be developed on a feature branch:
```bash
git checkout -b phase-1-1-input-batching
# ... implement and test ...
git checkout main
git merge phase-1-1-input-batching

# If issues found post-merge:
git revert {merge-commit-hash}
```

### Feature Flags (Optional)
For high-risk features (alternate screen, hot-reload):
```toml
[experimental]
enable_alternate_screen = false
enable_hot_reload = false
```

### Graceful Degradation
All new features should:
- Fall back to safe defaults on errors
- Log warnings instead of crashing
- Allow opt-out via config

---

## Open Questions & Future Work

### Unresolved Design Questions
1. **Session persistence across daemon restarts** - Store session state to disk? What about PTY process ownership?
2. **Mouse support** - Should sidebar be clickable? Terminal mouse forwarding?
3. **Copy/paste** - Native clipboard integration or tmux-style copy mode?
4. **Session grouping** - Tags, folders, or flat list only?
5. **Remote daemon** - SSH tunneling for remote session management?

### Beyond Milestone 2
- **Plugin system** - Lua/WASM scripts for custom session actions
- **Session sharing** - Multiple clients attached to same session (collaborative editing)
- **Scripting API** - Automate session creation via config file
- **Terminal recording** - Built-in session replay (like asciinema)
- **Network transparency** - Websocket daemon for web-based client

---

## Timeline & Resource Allocation

### Phase 1: Performance (1.5 weeks)
- Input batching: 3-4 days
- Adaptive render: 2-3 days
- PTY throttling: 2 days (optional)

### Phase 2: TUI Stability (2 weeks)
- Alternate screen: 5-7 days
- Parser persistence: 3-4 days
- Theme persistence: 2-3 days

### Phase 3: Session Details (1.5 weeks)
- Metadata bar: 2-3 days
- Git branch: 2-3 days
- Active app: 4-5 days (optional)

### Phase 4: Configuration (1 week)
- Config file: 3-4 days
- Input validation: 2 days
- Hot-reload: 3 days (optional)

**Total:** 4-6 weeks (assuming optional items skipped or deferred)

---

## Conclusion

Milestone 2 transforms mbulet from a functional MVP into a robust, performant, and feature-rich terminal multiplexer. The phased approach minimizes risk while delivering incremental value. Config file support (Phase 4.1) unlocks customization, performance improvements (Phase 1) ensure smooth UX at scale, and rich session details (Phase 3) enhance situational awareness.

**Next Steps:**
1. Review and approve this plan
2. Create GitHub issues/tickets for each milestone
3. Prioritize based on user feedback and use cases
4. Begin Phase 4.1 (Config File) to establish foundation
5. Proceed with Phase 1 (Performance) for immediate impact

**Success Criteria for Milestone 2:**
- ✅ All performance metrics achieved
- ✅ Fullscreen TUI apps work seamlessly
- ✅ Session metadata always visible
- ✅ User-configurable without recompile
- ✅ Zero regressions from Milestone 1
- ✅ Production-ready for daily use

---

**Document Version:** 1.0  
**Last Updated:** 2026-04-08  
**Status:** Ready for Implementation
