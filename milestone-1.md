# Milestone 1: Project History & Completed Work

## Project Overview

**mbulet** is a terminal multiplexer daemon/client system built in Rust, similar to tmux or screen, featuring:
- Background daemon managing persistent shell sessions
- TUI client with sidebar navigation and terminal pane
- Session switching, creation, deletion, renaming, and reordering
- Git worktree integration for fast branch-based session creation
- PTY output buffering with vt100 parser for terminal rendering
- OSC 7 CWD tracking with zero echo artifacts
- Centralized prefix key system (default: Ctrl+B)

---

## Architecture Foundations

### Core Components

#### 1. **Daemon (`src/daemon.rs`)**
- Unix socket server (`~/.local/share/mbulet/daemon.sock`)
- Auto-spawned on first client connection
- Manages session lifecycle (create, delete, rename, reorder)
- PTY subscriber system for live output streaming to attached clients
- CWD subscriber system for directory change notifications
- Atomic buffer replay on attach/detach
- SIGWINCH handling for terminal resize

#### 2. **Session (`src/session.rs`)**
- PTY wrapper using `portable-pty` crate
- Background thread reading PTY output continuously
- OSC 7 sequence extraction and CWD tracking
- Ring buffer (65KB) for buffered output replay
- Parser reset + buffer clear on dimension change
- Temporary ZDOTDIR bootstrap for zsh hook injection
- `resize_and_reset()` method for atomic attach operations

#### 3. **Client (`src/client.rs`)**
- ratatui TUI with crossterm backend
- Dual-focus mode: Sidebar (session list) and Terminal (active session)
- Per-session vt100 parsers for independent rendering
- Background message thread for daemon communication
- Keyboard event handling with prefix key system
- Relative layout sizing with runtime validation

#### 4. **Protocol (`src/protocol.rs`)**
- Length-prefixed JSON messages over Unix socket
- Bidirectional `ClientMsg` / `DaemonMsg` enums
- Session metadata: `SessionInfo { id, name, cwd }`
- `Attached { id, cleared }` flag for parser reset coordination

### Key Design Patterns

**Session Lifecycle:**
```
Client                  Daemon                  Session
  |                       |                       |
  |-- NewSession -------->|                       |
  |                       |-- create PTY -------->|
  |<-- SessionCreated ----|                       |
  |-- Attach ------------>|                       |
  |                       |-- resize_and_reset -->|
  |<-- PtyOutput ---------|<-- buffer replay -----|
  |<-- Attached ----------|                       |
```

**Attach Protocol:**
1. Client resets local parser (before sending Attach)
2. Client sends `Attach { id, cols, rows }`
3. Daemon calls `resize_and_reset(cols, rows)`
   - If dimensions changed: clears buffer, sends SIGWINCH, sets `cleared=true`
   - If unchanged: preserves buffer, sets `cleared=false`
4. Daemon sends `PtyOutput` with buffered content (if buffer not empty)
5. Daemon sends `CwdUpdate` (if CWD known)
6. Daemon sends `Attached { id, cleared }`
7. Client updates `attached_id` (parser already sized correctly)

---

## Critical Bug Fixes (Chronological)

### Bug #1: Disappearing Terminal Content on Session Switch

**Discovered:** During rapid session switching (3-5 times), terminal text would vanish or appear truncated.

**Root Causes:**
1. **Hard-coded pane size calculation** - `pane_size()` function used incorrect assumptions:
   - Hard-coded sidebar width: `22 + 2` (should be `22`)
   - Hard-coded bar heights: `2 + 2` (should be `1 + 1`)
   - Didn't match actual ratatui `Constraint::Min(0)` layout
   
2. **Unconditional parser reset on Attach** - Every `Attached` message triggered parser recreation:
   - Created race conditions with buffered output replay
   - Lost terminal state during same-size session switches
   - Parser dimensions could be stale when buffer replayed

**Fix Applied:** (See `BUGFIX_SUMMARY.md`)
1. **Rewrote `pane_size()` with relative sizing** (lines 149-163):
   ```rust
   let content_rows = rows.saturating_sub(1 + 1); // top + bottom bars
   let term_rows = content_rows.saturating_sub(2).max(1); // border overhead
   let content_cols = cols.saturating_sub(22); // sidebar width
   let term_cols = content_cols.saturating_sub(2).max(1); // border overhead
   ```

2. **Conditional parser reset** based on `cleared` flag (lines 237-251):
   ```rust
   DaemonMsg::Attached { id, cleared } => {
       app.attached_id = Some(id);
       if cleared {  // Only reset if server cleared buffer (size changed)
           let (tc, tr) = (app.term_cols, app.term_rows);
           if let Some(s) = app.sessions.iter().find(|s| s.id == id) {
               *s.parser.lock().unwrap() = vt100::Parser::new(tr.max(1), tc.max(1), 0);
           }
       }
   }
   ```

3. **Runtime size validation** in `draw_terminal()` (lines 754-773):
   ```rust
   let mut parser = session.parser.lock().unwrap();
   let (parser_rows, parser_cols) = parser.screen().size();
   if parser_rows != inner.height || parser_cols != inner.width {
       parser.set_size(inner.height, inner.width);
   }
   ```

**Files Changed:** `src/client.rs` (3 sections)

**Impact:** Eliminated content disappearance while preserving all existing functionality.

---

### Bug #2: Doubled Content on Session Reattach

**Discovered:** After switching away from a session and back, content appeared duplicated (especially in long-running Claude sessions).

**Root Cause:** Client-side vt100 parser was **not reset before sending Attach**, causing:
1. Session A attached, parser accumulates content
2. User switches to Session B (detaches from A)
3. Session A continues processing `PtyOutput` messages → parser keeps growing
4. User switches back to Session A
5. Client sends `Attach { id: A, cols, rows }`
6. Server sends buffered output via `PtyOutput`
7. Client **appends** buffered output to existing parser content → **duplication**
8. Client receives `Attached` → no action (parser already dirty)

**Key Insight:** `PtyOutput` arrives **before** `Attached`, so resetting the parser in the `Attached` handler would erase the content that was just replayed.

**Fix Applied:** (See `DOUBLED_CONTENT_FIX.md`)

Reset the parser **before sending Attach**, not after receiving `Attached`:

1. **Initial attach on startup** (lines 280-292):
   ```rust
   if let Some(id) = id {
       // Reset parser before sending initial attach
       {
           let app = app.lock().unwrap();
           if let Some(s) = app.sessions.iter().find(|s| s.id == id) {
               *s.parser.lock().unwrap() = vt100::Parser::new(tr.max(1), tc.max(1), 0);
           }
       }
       send_msg(&mut *stream_write.lock().unwrap(), &ClientMsg::Attach { id, cols: tc, rows: tr })?;
   }
   ```

2. **Session switch via Action::SendMsg** (lines 481-491):
   ```rust
   Action::SendMsg(msg) => {
       if let ClientMsg::Attach { id, cols, rows } = &msg {
           let app = app.lock().unwrap();
           if let Some(s) = app.sessions.iter().find(|s| s.id == *id) {
               *s.parser.lock().unwrap() = vt100::Parser::new(*rows.max(&1), *cols.max(&1), 0);
           }
       }
       let _ = send_msg(&mut *stream_write.lock().unwrap(), &msg);
   }
   ```

3. **Attached handler** (lines 247-252):
   ```rust
   DaemonMsg::Attached { id, cleared: _ } => {
       let mut app = app.lock().unwrap();
       app.attached_id = Some(id);
       // Parser was already reset when Attach was sent (before server
       // replayed buffered output), so no action needed here.
   }
   ```

**Files Changed:** `src/client.rs` (3 small edits)

**Impact:** Eliminated content duplication with minimal invasive changes.

**Edge Cases Handled:**
- Normal reattach (size unchanged): ✅ Single copy of content
- Size change (SIGWINCH triggered): ✅ Clean redraw at new size
- Empty buffer (after `clear` command): ✅ Blank screen as expected

---

## Feature Implementations

### Feature #1: Session Move (Reordering)

**Implementation:** (Documented in `MANUAL_TEST_MOVE.md`)

**Capabilities:**
- **Sidebar focus**: `Ctrl+B Up/Down` moves selected session up/down in list
- **Order persists daemon-side** via `ReorderSession` message
- **Graceful edge cases**: First/last sessions cannot move beyond bounds
- **Selection stability**: Selected session highlight follows moved item

**Protocol Addition:**
```rust
// Client → Daemon
ClientMsg::ReorderSession { id: usize, new_index: usize }

// Daemon → Client
DaemonMsg::SessionReordered { id: usize, new_index: usize }
```

**Files Changed:** 
- `src/protocol.rs`: Added `ReorderSession` / `SessionReordered` messages
- `src/daemon.rs`: Implemented reorder logic with bounds checking
- `src/client.rs`: 
  - `move_session_up()` / `move_session_down()` methods (lines 122-146)
  - Sidebar keybindings for `Ctrl+B Up/Down` (lines 392-434)

**Testing:** Manual test guide created (`MANUAL_TEST_MOVE.md`)

---

### Feature #2: Direct Session Switching from Terminal Focus

**Implementation:** (Documented in `SESSION_SWITCHING_IMPLEMENTATION.md`)

**Capabilities:**
- **Terminal focus**: `Ctrl+B Up/Down` switches to previous/next session and attaches
- **Wraps around**: First → Last (on Up), Last → First (on Down)
- **Sidebar sync**: Sidebar selection automatically follows current session
- **Empty/single session handling**: Graceful no-ops

**Implementation Details:**
- When in Terminal focus and prefix is pending:
  - `Up` key calls `app.prev()` to update selection and `list_state`
  - `Down` key calls `app.next()` to update selection and `list_state`
  - Both trigger immediate `ClientMsg::Attach` to the new session
  - Parser is reset before attach (handled by existing code)

**Files Changed:**
- `src/client.rs`: Terminal focus keybindings (lines 392-434)
- Footer legend updated to show `^B ↑/↓: switch session` (lines 730-738)

**Preserved Behavior:**
- Sidebar focus still uses `Ctrl+B Up/Down` for reordering
- All other shortcuts remain functional

---

### Feature #3: Centralized Prefix Key System

**Design:** Single-point configuration for all prefix key references.

**Constants** (lines 25-27):
```rust
const PREFIX_KEY: char = 'b';
const PREFIX_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL;
```

**All prefix detection uses these constants:**
- Line 435: Initial prefix detection
- Line 417: Double-prefix detection for sending literal prefix
- Lines 683-689, 699, 725-728, 732-737: Footer display

**To change prefix:** Modify `PREFIX_KEY` and/or `PREFIX_MODIFIERS` constants only.

**Testing:** Manual test guide includes prefix change verification (see `MANUAL_TEST_MOVE.md` step E).

---

### Feature #4: Git Worktree Session Creation

**Purpose:** Fast session creation for git branches without full clones.

**User Flow:**
1. In sidebar, press `Ctrl+B w`
2. Enter branch name (e.g., `feature-xyz`)
3. Client executes: `git worktree add ../feature-xyz -b feature-xyz && cd ../feature-xyz`
4. New session created with shell already in worktree directory

**Implementation:**
- Uses `startup_cmds` parameter in `Session::new()`
- Commands are sent as normal input (echo visible)
- CWD inherited from parent directory or detected via OSC 7

**Files Changed:**
- `src/client.rs`: Worktree mode state and input handling (lines 461-498)
- Footer shows `^B w: worktree` prompt when in sidebar

---

### Feature #5: OSC 7 CWD Tracking (Zero Echo Artifacts)

**Purpose:** Track current working directory without visible shell commands.

**Approach:** Inject precmd hook via temporary ZDOTDIR bootstrap.

**Implementation** (`src/session.rs` lines 103-121):
1. Create temp directory: `/tmp/mbulet-zdotdir-{id}/`
2. Write `.zshrc` with:
   - `__mbulet_precmd()` function emitting OSC 7
   - `precmd_functions+=(__mbulet_precmd)` registration
   - `source ~/.zshrc` to load user config
   - `PROMPT_SP=""` to disable partial line marker
3. Set `ZDOTDIR` env var to temp directory
4. Shell sources mbulet's `.zshrc` first, then user's real `.zshrc`

**OSC 7 Format:** `\x1b]7;file://hostname/path\x07` or `\x1b]7;file://hostname/path\x1b\\`

**Parsing** (`extract_osc7()` function):
- Scans PTY output for OSC 7 sequences
- Extracts path from `file://hostname/path` URL
- Strips sequences before passing to vt100 parser (prevents garbage rendering)
- Notifies CWD subscribers with new path

**Protocol Integration:**
```rust
DaemonMsg::CwdUpdate { id: usize, cwd: String }
```

**Files Changed:**
- `src/session.rs`: Bootstrap creation, OSC 7 extraction, CWD subscriber system
- `src/daemon.rs`: Send `CwdUpdate` on attach and when OSC 7 detected
- `src/protocol.rs`: `SessionInfo` includes `cwd: Option<String>`
- `src/client.rs`: Update client-side `cwd` field on `CwdUpdate` message

---

## Current Shortcuts & Keybindings

### Prefix Key: `Ctrl+B` (configurable via constants)

**Sidebar Focus:**
- `j` / `k` / `Up` / `Down`: Navigate sessions
- `n`: Create new session (prompt for name)
- `r`: Rename selected session
- `d`: Delete selected session (cannot delete last session)
- `Ctrl+B Up`: Move session up in list
- `Ctrl+B Down`: Move session down in list
- `Ctrl+B w`: Create git worktree session (prompt for branch name)
- `Ctrl+B Tab`: Switch to terminal focus
- `Ctrl+B d`: Detach from client
- `Ctrl+B q`: Shutdown daemon
- `Ctrl+B Ctrl+B`: Send literal `Ctrl+B` to terminal (double-prefix)

**Terminal Focus:**
- `Ctrl+B Up`: Switch to previous session (with wrap-around)
- `Ctrl+B Down`: Switch to next session (with wrap-around)
- `Ctrl+B Tab`: Switch to sidebar focus
- `Ctrl+B d`: Detach from client
- `Ctrl+B q`: Shutdown daemon
- `Ctrl+B Ctrl+B`: Send literal `Ctrl+B` to terminal (double-prefix)
- All other keys: Forwarded to PTY as normal input

---

## Technical Achievements

### 1. **Parser/Render Size Synchronization**
- Relative pane sizing matching ratatui constraints
- Conditional parser reset based on server-side `cleared` flag
- Runtime size validation before rendering
- Eliminates parser/render desync issues

### 2. **Clean Attach/Detach Flow**
- Client-side parser reset before `Attach` (prevents duplication)
- Server-side atomic `resize_and_reset()` (prevents partial state)
- Buffered output replay for same-size reattach
- SIGWINCH for size-changed reattach

### 3. **Zero-Echo CWD Tracking**
- ZDOTDIR bootstrap technique
- OSC 7 extraction and stripping before vt100 parsing
- Pub/sub pattern for CWD updates
- No visible artifacts in shell output

### 4. **Prefix Key Centralization**
- Single-point configuration (2 constants)
- All keybindings reference constants
- Footer display auto-adapts to configured prefix
- Easy customization without scattered edits

### 5. **Dual Focus Model**
- Context-dependent keybindings (same prefix, different behavior)
- Sidebar: Session management operations
- Terminal: Session switching operations
- Clean separation of concerns

---

## Protocol Evolution

### Initial Protocol (First Commit)
```rust
// Client → Daemon
ClientMsg::ListSessions
ClientMsg::NewSession { name, cols, rows }
ClientMsg::DeleteSession { id }
ClientMsg::RenameSession { id, name }
ClientMsg::Attach { id, cols, rows }
ClientMsg::Detach
ClientMsg::Input { data }
ClientMsg::Resize { cols, rows }
ClientMsg::Shutdown

// Daemon → Client
DaemonMsg::SessionList { sessions }
DaemonMsg::SessionCreated { id, name }
DaemonMsg::SessionDeleted { id }
DaemonMsg::SessionRenamed { id, name }
DaemonMsg::PtyOutput { id, data }
DaemonMsg::Attached { id }
DaemonMsg::Detached
DaemonMsg::Ok
DaemonMsg::Error { msg }
```

### Protocol Extensions (During Project)

**Added to `SessionInfo`:**
```rust
pub struct SessionInfo {
    pub id: usize,
    pub name: String,
    pub cwd: Option<String>,  // NEW: For CWD tracking
}
```

**Added to `ClientMsg`:**
```rust
ClientMsg::NewSession {
    name: String,
    cols: u16,
    rows: u16,
    startup_cmds: Vec<String>,  // NEW: For git worktree integration
}
ClientMsg::ReorderSession { id: usize, new_index: usize }  // NEW
```

**Added to `DaemonMsg`:**
```rust
DaemonMsg::CwdUpdate { id: usize, cwd: String }  // NEW
DaemonMsg::SessionReordered { id: usize, new_index: usize }  // NEW
DaemonMsg::Attached { 
    id: usize, 
    cleared: bool  // MODIFIED: Added flag for parser reset coordination
}
```

---

## Current Known Limitations

### Performance
1. **Paste performance not optimized** - Large paste operations may be slow due to per-byte input forwarding and render loop frequency.
2. **No input batching** - Each key event triggers separate `ClientMsg::Input` message.
3. **Render loop runs at fixed rate** - No adaptive frame rate based on activity.

### Session Metadata
1. **No session name display in terminal** - Only visible in sidebar.
2. **No active app/agent detection** - Cannot detect if codex/opencode/claude is running.
3. **No git branch display** - CWD tracked but not parsed for git status.
4. **No session age/uptime** - No timestamp tracking for session creation.

### Rich TUI Stability
1. **No theme persistence** - Client restarts with default colors.
2. **No alternate screen handling** - Apps using alternate screen (vim, less) may not reattach cleanly.
3. **Fullscreen TUI apps** (htop, nvim) - May lose state on detach/reattach depending on app behavior.

### Configuration
1. **No config file** - All settings hard-coded in source.
2. **Prefix key requires recompile** - No runtime configuration.
3. **Sidebar width hard-coded** (22 columns) - Not configurable.
4. **No color customization** - Hard-coded color scheme.

### Session Management
1. **Cannot delete last session** - Intentional safety limit, but inflexible.
2. **No session grouping/tags** - Flat list only.
3. **No session persistence across daemon restart** - All sessions lost on shutdown.
4. **No session import/export** - Cannot save/restore session configuration.

### Terminal Emulation
1. **vt100 parser limitations** - May not support all modern terminal features.
2. **No clipboard integration** - Cannot copy/paste across sessions via TUI.
3. **No mouse support** - Keyboard-only navigation.

### Error Handling
1. **No graceful degradation** - Client exits on daemon disconnect.
2. **Limited error messages** - Generic errors for most failure cases.
3. **No retry logic** - Connection failures are fatal.

---

## Rendering & Session Switch Iterations

### Iteration 1: Initial Implementation (First Commit)
- Basic ratatui TUI with sidebar and terminal pane
- Hard-coded pane size calculation
- Unconditional parser reset on `Attached`
- **Problem:** Content disappears after 3-5 session switches

### Iteration 2: Relative Sizing Fix
- Rewrote `pane_size()` to match actual layout constraints
- Fixed bar heights (1 each instead of 2)
- Fixed sidebar width (22 instead of 24)
- **Problem:** Still some desync issues during rapid switching

### Iteration 3: Conditional Parser Reset
- Added `cleared` flag to `Attached` message
- Only reset client parser if server cleared buffer (size changed)
- Preserved parser state during same-size switches
- **Problem:** Content duplication on reattach

### Iteration 4: Pre-Attach Parser Reset
- Moved parser reset to **before** sending `Attach` message
- Reset happens in two places: initial attach and session switch
- `Attached` handler no longer touches parser
- **Result:** ✅ Eliminated all duplication and desync issues

### Iteration 5: Runtime Size Validation (Safety Net)
- Added just-in-time size check in `draw_terminal()`
- Corrects any residual mismatches before rendering
- Acts as defensive programming layer
- **Result:** ✅ Robust against edge cases

---

## Build & Test Status

### Build Configuration
- **Edition:** Rust 2024
- **Profile:** Debug and Release builds both successful
- **Dependencies:**
  - `ratatui` 0.29 - TUI framework
  - `crossterm` 0.28 - Terminal backend
  - `portable-pty` 0.8 - PTY management
  - `vt100` 0.15 - Terminal emulator parser
  - `serde` 1.x - Serialization
  - `serde_json` 1.x - JSON protocol
  - `dirs` 5.x - Home directory detection

### Test Artifacts
- `test_session_switch.sh` - Manual session switching test procedure
- `test_session_move.sh` - Manual session reordering test
- `MANUAL_TEST_MOVE.md` - Comprehensive test guide for move/prefix features

### Testing Coverage (Manual)
- ✅ Session creation and deletion
- ✅ Session renaming
- ✅ Session reordering (up/down)
- ✅ Session switching (terminal focus)
- ✅ Session switching (sidebar focus via attach)
- ✅ Detach and reattach (same size)
- ✅ Detach and reattach (size changed)
- ✅ Terminal resize handling
- ✅ Prefix key customization
- ✅ Git worktree session creation
- ✅ CWD tracking via OSC 7
- ✅ Empty session list handling
- ✅ Single session handling
- ✅ Rapid session switching (10+ cycles)
- ✅ Content duplication prevention
- ✅ Parser/render size synchronization

---

## Repository State

### Git History
- Single commit: `ab5c144` (first commit)
- Branch: `main` (synced with `origin/main`)

### File Structure
```
/Users/prima.adimekari.com/work/mbulet/
├── Cargo.toml                              # Package manifest
├── Cargo.lock                              # Dependency lock
├── .gitignore                              # Ignores /target
├── src/
│   ├── main.rs                             # Entry point, daemon spawning
│   ├── daemon.rs                           # Session manager, protocol handler
│   ├── session.rs                          # PTY wrapper, OSC 7 tracking
│   ├── client.rs                           # TUI application, keybindings
│   └── protocol.rs                         # Message types, send/recv
├── test_session_switch.sh                  # Manual test script
├── test_session_move.sh                    # Manual test script
├── BUGFIX_SUMMARY.md                       # Bug #1 documentation
├── DOUBLED_CONTENT_FIX.md                  # Bug #2 documentation
├── SESSION_SWITCHING_IMPLEMENTATION.md     # Feature #2 documentation
└── MANUAL_TEST_MOVE.md                     # Feature #1 test guide
```

---

## Summary of Completed Work

### Architecture
✅ Daemon/client model with Unix socket communication  
✅ Length-prefixed JSON protocol  
✅ Per-session PTY management with vt100 parsers  
✅ Pub/sub pattern for PTY output and CWD updates  
✅ Ring buffer for session output replay  

### Features
✅ Session creation, deletion, renaming, reordering  
✅ Session switching from terminal focus (Ctrl+B Up/Down)  
✅ Git worktree integration for branch sessions  
✅ OSC 7 CWD tracking with zero echo artifacts  
✅ Centralized prefix key configuration  
✅ Dual-focus mode (sidebar vs. terminal)  

### Bug Fixes
✅ Disappearing content on session switch (relative sizing + conditional reset)  
✅ Doubled content on reattach (pre-attach parser reset)  

### Testing
✅ Manual test procedures documented  
✅ Prefix key customization verified  
✅ Edge cases handled (empty list, single session, rapid switching)  

### Documentation
✅ Detailed bugfix write-ups  
✅ Feature implementation summaries  
✅ Manual test guides  
✅ This comprehensive milestone document  

---

**Project Status:** Stable, feature-complete for MVP use cases. Ready for Milestone 2 enhancements.
