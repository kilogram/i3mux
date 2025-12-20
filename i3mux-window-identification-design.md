# i3mux Window Identification Design Document

## Executive Summary

This document analyzes approaches for reliably identifying terminal windows spawned by i3mux in the i3 window manager environment. After evaluating PID-based approaches, X11 properties, and i3-specific mechanisms, the recommended solution is a **hybrid approach** combining i3 IPC window events with terminal-specific WM_CLASS configuration, with fallback strategies for unsupported terminals.

---

## Problem Statement

When i3mux spawns a terminal emulator (xterm, alacritty, urxvt, kitty, etc.), it must:

1. Detect when the window appears
2. Identify which specific window was spawned (not another user-opened window)
3. Apply an i3 mark to it (e.g., `_i3mux:local:ws1-001`)

### Current Challenges

1. **PID mismatch**: Terminal emulators fork, so `Command::new().spawn().id()` returns the parent PID, not the X11 client PID
2. **Terminal heterogeneity**: Different terminals have different CLI options for setting WM_CLASS
3. **Race conditions**: "Find newest window" approaches fail with concurrent window creation
4. **Container environments**: Docker/namespace isolation affects PID visibility

---

## Approach Analysis

### 1. PID-Based Approaches

#### 1.1 Direct PID Matching

**Mechanism**: Match `child.id()` from Rust's `Command::spawn()` against i3's `get_tree` node PIDs.

**Why it fails**:
- Terminal emulators typically fork: the spawned PID is the *parent* process, while the X11 window is created by a *child* process
- i3's `window` nodes contain `pid` from `_NET_WM_PID` X property, which is set by the X11 client (the forked child)
- In containerized environments, PID namespaces may differ between i3mux and the terminal

**i3 get_tree structure**:
```json
{
  "window": 12345678,           // X11 window ID
  "window_properties": {
    "class": "Alacritty",
    "instance": "alacritty", 
    "title": "Terminal"
  },
  "pid": 1234                   // From _NET_WM_PID, NOT spawn PID
}
```

**Verdict**: ❌ Not reliable without process tree traversal

#### 1.2 Process Tree Traversal

**Mechanism**: After spawning, traverse `/proc` to find child/descendant processes, then match against window PIDs.

**Implementation sketch**:
```rust
fn find_descendant_pids(parent_pid: u32) -> Vec<u32> {
    let mut descendants = vec![];
    let mut to_check = vec![parent_pid];
    
    while let Some(pid) = to_check.pop() {
        // Read /proc/*/stat to find processes with this ppid
        for entry in fs::read_dir("/proc").unwrap() {
            if let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", entry.path().file_name())) {
                // Parse stat file, field 4 is PPID
                let fields: Vec<&str> = stat.split_whitespace().collect();
                if let Ok(ppid) = fields.get(3).and_then(|s| s.parse::<u32>().ok()) {
                    if ppid == pid {
                        let child_pid = fields[0].parse::<u32>().unwrap();
                        descendants.push(child_pid);
                        to_check.push(child_pid);
                    }
                }
            }
        }
    }
    descendants
}
```

**Pros**:
- Works regardless of terminal emulator
- Doesn't require terminal-specific configuration

**Cons**:
- Race condition between fork and window creation
- Linux-specific (`/proc` filesystem)
- Expensive to scan all processes repeatedly
- PID reuse risk in long-running systems
- Fails in PID namespace isolation scenarios

**Verdict**: ⚠️ Possible fallback, but complex and fragile

---

### 2. X11 Property-Based Approaches

#### 2.1 WM_CLASS (Instance/Class) Matching

**Mechanism**: Configure terminal to set a unique WM_CLASS instance, then match via i3 criteria.

**Terminal Support Matrix**:

| Terminal   | Set Instance         | Set Class           | Example                                        |
|------------|----------------------|---------------------|------------------------------------------------|
| xterm      | `-name <instance>`   | `-class <class>`    | `xterm -name i3mux:session1`                   |
| alacritty  | `--class <general>,<instance>` | Same flag  | `alacritty --class Alacritty,i3mux:session1`   |
| kitty      | `--name <instance>`  | `--class <class>`   | `kitty --name i3mux:session1`                  |
| urxvt      | `-name <instance>`   | Xresources only     | `urxvt -name i3mux:session1`                   |
| gnome-term | Not supported        | Not supported       | ❌                                              |
| konsole    | Not supported        | Not supported       | ❌                                              |

**Implementation**:
```rust
fn spawn_terminal_with_instance(terminal: &str, instance: &str, cmd: &str) -> io::Result<Child> {
    let args = match terminal {
        "xterm" => vec!["-name", instance, "-e", cmd],
        "alacritty" => vec!["--class", &format!("Alacritty,{}", instance), "-e", cmd],
        "kitty" => vec!["--name", instance, "-e", cmd],
        "urxvt" => vec!["-name", instance, "-e", cmd],
        _ => return Err(io::Error::new(io::ErrorKind::Unsupported, "Terminal not supported")),
    };
    Command::new(terminal).args(&args).spawn()
}
```

**i3 matching**:
```rust
// After window appears with matching instance
i3_connection.run_command(&format!(
    "[instance=\"{}\"] mark --add {}", 
    instance, 
    mark_name
))?;
```

**Pros**:
- Simple and deterministic
- Races only with identically-named instances (under our control)
- Well-supported by major terminals

**Cons**:
- Requires terminal-specific argument handling
- Some terminals (GNOME Terminal, Konsole) don't support CLI instance setting
- Shell title changes can interfere (but instance persists)

**Verdict**: ✅ Primary approach for supported terminals

#### 2.2 Custom X11 Atom Property

**Mechanism**: After window appears, set a custom X11 property (e.g., `_I3MUX_SESSION`) on it.

**Challenge**: How to identify which window to set the property on *before* we've identified it?

**Solution**: Combine with i3 window event subscription - when a new window appears, check if it matches expected criteria, then set property.

**Setting property with xprop**:
```bash
xprop -id <window_id> -format _I3MUX_SESSION 8s -set _I3MUX_SESSION "session:local:ws1-001"
```

**Setting property with x11rb (Rust)**:
```rust
use x11rb::protocol::xproto::{AtomEnum, PropMode};

fn set_i3mux_property(conn: &impl Connection, window: u32, session_id: &str) -> Result<()> {
    // Intern our custom atom
    let atom = conn.intern_atom(false, b"_I3MUX_SESSION")?.reply()?.atom;
    
    conn.change_property(
        PropMode::REPLACE,
        window,
        atom,
        AtomEnum::STRING.into(),
        8,
        session_id.len() as u32,
        session_id.as_bytes(),
    )?;
    
    conn.flush()?;
    Ok(())
}
```

**Pros**:
- Terminal-agnostic once window is found
- Property persists through title changes
- Can store rich metadata

**Cons**:
- Requires finding window first (chicken-and-egg)
- Additional X11 connection needed

**Verdict**: ⚠️ Good for secondary identification, not initial detection

---

### 3. i3-Specific Mechanisms

#### 3.1 i3 IPC Window Events

**Mechanism**: Subscribe to i3's `window` event stream and react to `new` window events.

**Event structure** (when `change: "new"`):
```json
{
  "change": "new",
  "container": {
    "id": 94512345678,
    "window": 12345678,
    "window_properties": {
      "class": "Alacritty",
      "instance": "i3mux:local:ws1-001",
      "title": "Terminal"
    },
    "focused": false,
    "marks": []
  }
}
```

**Implementation with i3ipc crate**:
```rust
use i3ipc::{I3EventListener, Subscription, event::Event};
use std::sync::mpsc;

struct WindowWatcher {
    expected_instances: HashMap<String, oneshot::Sender<u64>>,
}

impl WindowWatcher {
    fn watch_for_window(&mut self, instance: &str) -> oneshot::Receiver<u64> {
        let (tx, rx) = oneshot::channel();
        self.expected_instances.insert(instance.to_string(), tx);
        rx
    }
    
    fn handle_event(&mut self, event: Event) {
        if let Event::WindowEvent(e) = event {
            if e.change == "new" {
                if let Some(props) = &e.container.window_properties {
                    if let Some(tx) = self.expected_instances.remove(&props.instance) {
                        let _ = tx.send(e.container.id as u64);
                    }
                }
            }
        }
    }
}

// Main event loop
fn spawn_event_listener(mut watcher: WindowWatcher) {
    let mut listener = I3EventListener::connect().unwrap();
    listener.subscribe(&[Subscription::Window]).unwrap();
    
    for event in listener.listen() {
        watcher.handle_event(event.unwrap());
    }
}
```

**Pros**:
- Real-time notification when windows appear
- Direct access to window properties at creation time
- Deterministic matching via instance name

**Cons**:
- Requires persistent IPC connection
- Events can be missed if listener starts after window creation

**Verdict**: ✅ Essential component of the solution

#### 3.2 i3 Marks with for_window Rules

**Mechanism**: Pre-configure i3 with dynamic rules to auto-mark windows.

**Problem**: `for_window` rules are static in config, can't be added dynamically at runtime.

**Alternative**: Use command-mode criteria matching after detection:
```rust
// After window ID is known
i3_conn.run_command(&format!(
    "[con_id={}] mark --add {}", 
    container_id, 
    "_i3mux:local:ws1-001"
))?;
```

**Verdict**: ✅ Used for applying marks, not for detection

---

### 4. Hybrid Approaches

#### 4.1 Event-Driven Instance Matching (Recommended)

**Architecture**:

```
┌─────────────────────────────────────────────────────────────────────┐
│                          i3mux Controller                            │
├─────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  ┌───────────────────┐     ┌────────────────────────────────────┐   │
│  │  Terminal Spawner │     │  i3 Event Subscriber               │   │
│  │                   │     │                                    │   │
│  │  1. Generate UUID │     │  - Subscribes to window events     │   │
│  │  2. Build args    │────▶│  - Maintains expected_windows map  │   │
│  │  3. Register UUID │     │  - Matches instance on "new"       │   │
│  │  4. Spawn process │     │  - Notifies spawner of match       │   │
│  └───────────────────┘     └────────────────────────────────────┘   │
│           │                              │                           │
│           │                              │                           │
│           ▼                              ▼                           │
│  ┌───────────────────┐     ┌────────────────────────────────────┐   │
│  │ Terminal Process  │     │  Mark Applicator                   │   │
│  │                   │     │                                    │   │
│  │ WM_CLASS instance:│     │  i3-msg "[con_id=X] mark Y"        │   │
│  │ _i3mux_<uuid>     │────▶│                                    │   │
│  └───────────────────┘     └────────────────────────────────────┘   │
│                                                                      │
└─────────────────────────────────────────────────────────────────────┘
```

**Workflow**:

1. **Generate unique identifier**: UUID or counter-based ID
2. **Register with event subscriber**: Before spawning, tell event loop what instance to expect
3. **Spawn terminal**: With instance set via CLI args
4. **Event subscriber matches**: On `window::new` event, check `window_properties.instance`
5. **Apply mark**: Once matched, run i3 command to add mark
6. **Cleanup**: Remove from expected list, timeout after N seconds for failed spawns

**Instance naming convention**:
```
_i3mux_<uuid>_<session>_<workspace>_<index>
Example: _i3mux_a1b2c3d4_local_ws1_001
```

Using underscore prefix ensures the instance is unlikely to conflict with user-launched terminals.

---

## Recommended Solution

### Primary Strategy: WM_CLASS Instance + i3 Events

```rust
use i3ipc::{I3Connection, I3EventListener, Subscription};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use uuid::Uuid;

pub struct I3MuxWindowManager {
    i3_conn: I3Connection,
    pending_windows: Arc<Mutex<HashMap<String, oneshot::Sender<WindowInfo>>>>,
    terminal_config: TerminalConfig,
}

struct WindowInfo {
    container_id: i64,
    window_id: u32,
}

struct TerminalConfig {
    terminal: String,  // e.g., "alacritty"
}

impl I3MuxWindowManager {
    pub async fn spawn_and_mark(&self, session: &str, workspace: &str, index: u32) 
        -> Result<WindowInfo, Error> 
    {
        // 1. Generate unique instance identifier
        let uuid = Uuid::new_v4().to_string()[..8].to_string();
        let instance = format!("_i3mux_{}_{}_{}_{:03}", uuid, session, workspace, index);
        let mark = format!("_i3mux:{}:{}:{:03}", session, workspace, index);
        
        // 2. Create oneshot channel for window notification
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_windows.lock().unwrap();
            pending.insert(instance.clone(), tx);
        }
        
        // 3. Build terminal command with instance
        let wrapper_script = self.build_wrapper_script(session, workspace, index)?;
        let args = self.terminal_args(&instance, &wrapper_script);
        
        // 4. Spawn terminal
        let _child = Command::new(&self.terminal_config.terminal)
            .args(&args)
            .spawn()?;
        
        // 5. Wait for window with timeout
        let window_info = tokio::time::timeout(
            Duration::from_secs(10),
            rx
        ).await??;
        
        // 6. Apply i3 mark
        self.i3_conn.run_command(&format!(
            "[con_id={}] mark --add {}", 
            window_info.container_id, 
            mark
        ))?;
        
        Ok(window_info)
    }
    
    fn terminal_args(&self, instance: &str, wrapper_script: &str) -> Vec<String> {
        match self.terminal_config.terminal.as_str() {
            "xterm" => vec![
                "-name".into(), instance.into(),
                "-e".into(), wrapper_script.into()
            ],
            "alacritty" => vec![
                "--class".into(), format!("Alacritty,{}", instance),
                "-e".into(), wrapper_script.into()
            ],
            "kitty" => vec![
                "--name".into(), instance.into(),
                "-e".into(), wrapper_script.into()
            ],
            "urxvt" => vec![
                "-name".into(), instance.into(),
                "-e".into(), wrapper_script.into()
            ],
            _ => vec!["-e".into(), wrapper_script.into()]  // Fallback: no instance
        }
    }
}

// Separate task: i3 event listener
async fn window_event_listener(
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<WindowInfo>>>>
) {
    let mut listener = I3EventListener::connect().unwrap();
    listener.subscribe(&[Subscription::Window]).unwrap();
    
    for event in listener.listen() {
        if let Ok(Event::WindowEvent(e)) = event {
            if e.change == "new" {
                if let Some(props) = &e.container.window_properties {
                    let mut pending = pending.lock().unwrap();
                    if let Some(tx) = pending.remove(&props.instance) {
                        let _ = tx.send(WindowInfo {
                            container_id: e.container.id,
                            window_id: e.container.window.unwrap_or(0) as u32,
                        });
                    }
                }
            }
        }
    }
}
```

### Fallback Strategy: Title-Based Detection (for unsupported terminals)

For terminals that don't support instance setting (GNOME Terminal, Konsole):

1. Set a unique window title via the shell inside the terminal
2. Match on title in i3 events
3. Apply mark immediately, then the wrapper script can change title

```rust
fn fallback_spawn(&self, session: &str, workspace: &str, index: u32) -> Result<WindowInfo, Error> {
    let uuid = Uuid::new_v4().to_string()[..8].to_string();
    let marker_title = format!("_i3mux_init_{}", uuid);
    
    // Wrapper script that sets title on startup
    let wrapper = format!(r#"#!/bin/bash
printf '\033]0;{}\007'  # Set window title
exec abduco -A {} bash
"#, marker_title, session_name);
    
    // Register for title match
    self.register_pending_title(&marker_title, tx);
    
    // Spawn terminal
    Command::new(&self.terminal).args(&["-e", &wrapper_path]).spawn()?;
    
    // Wait for match on title
    rx.await
}

// In event listener, also check title
if props.title.starts_with("_i3mux_init_") {
    if let Some(tx) = pending_titles.remove(&props.title) {
        // ...
    }
}
```

### Secondary Fallback: Newest Window Heuristic

If no match is found within timeout:

1. Get window list before spawn
2. Spawn terminal
3. Poll for new windows matching terminal class
4. Select the newest one not in the original list

```rust
async fn fallback_newest_window(&self, terminal_class: &str) -> Result<WindowInfo, Error> {
    // Get current window IDs
    let before: HashSet<_> = self.get_all_window_ids().await?.into_iter().collect();
    
    // Spawn terminal
    Command::new(&self.terminal).args(&["-e", &wrapper]).spawn()?;
    
    // Poll for new window
    for _ in 0..50 {  // 5 seconds with 100ms intervals
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        let tree = self.i3_conn.get_tree()?;
        for node in tree.iter_windows() {
            if !before.contains(&node.id) {
                if node.window_properties.class == terminal_class {
                    return Ok(WindowInfo {
                        container_id: node.id,
                        window_id: node.window.unwrap_or(0) as u32,
                    });
                }
            }
        }
    }
    
    Err(Error::WindowNotFound)
}
```

---

## Implementation Recommendations

### Phase 1: Core Implementation

1. Implement i3 event listener as a separate async task
2. Create terminal configuration abstraction with per-terminal CLI mappings
3. Implement primary spawn-with-instance workflow
4. Add timeout handling and error recovery

### Phase 2: Fallback Strategies

1. Add title-based fallback for unsupported terminals
2. Implement newest-window heuristic as last resort
3. Add process tree traversal for edge cases (optional)

### Phase 3: Robustness

1. Handle event listener disconnection and reconnection
2. Implement mark collision detection
3. Add metrics/logging for debugging
4. Handle container (Docker/Podman) scenarios

### Testing Strategy

1. **Unit tests**: Mock i3 IPC responses
2. **Integration tests**: Docker environment with Xephyr (as you have)
3. **Terminal coverage**: Test with xterm, alacritty, kitty, urxvt at minimum

---

## Appendix A: Terminal CLI Reference

### xterm
```bash
xterm -name <instance> -class <class> -title <title> -e <command>
```
- WM_CLASS: `<instance>, <class>`

### alacritty  
```bash
alacritty --class <class>,<instance> --title <title> -e <command>
```
- WM_CLASS: `<instance>, <class>`
- Note: Order is `class,instance` not `instance,class`

### kitty
```bash
kitty --name <instance> --class <class> --title <title> -e <command>
```
- WM_CLASS: `<instance>, <class>`

### urxvt
```bash
urxvt -name <instance> -title <title> -e <command>
```
- WM_CLASS: `<instance>, URxvt`
- Class is not settable via CLI

---

## Appendix B: i3 IPC Window Event Reference

**Event subscription**:
```json
["window"]
```

**Event payload (change: "new")**:
```json
{
  "change": "new",
  "container": {
    "id": 94350918238176,
    "type": "con",
    "window": 23068678,
    "window_properties": {
      "class": "Alacritty",
      "instance": "_i3mux_a1b2c3d4",
      "title": "Terminal",
      "transient_for": null,
      "window_role": null,
      "window_type": "normal"
    },
    "marks": [],
    "focused": true,
    "urgent": false
  }
}
```

**Key fields**:
- `container.id`: i3's internal container ID (use for criteria matching with `con_id`)
- `container.window`: X11 window ID
- `container.window_properties.instance`: WM_CLASS instance (what we match on)
- `container.window_properties.class`: WM_CLASS class

---

## Appendix C: Rust Crate Recommendations

| Purpose | Crate | Notes |
|---------|-------|-------|
| i3 IPC | `i3ipc` or `swayipc` | Event subscription and commands |
| Async runtime | `tokio` | For event loop and timeouts |
| X11 (optional) | `x11rb` | For custom atom properties |
| UUID generation | `uuid` | For unique instance IDs |

---

## Summary

The recommended approach combines:

1. **Primary**: Terminal-specific WM_CLASS instance configuration + i3 window event subscription
2. **Fallback 1**: Title-based matching for unsupported terminals
3. **Fallback 2**: Newest-window heuristic with class matching

This provides reliable, race-condition-resistant window identification while supporting a wide range of terminal emulators and handling edge cases gracefully.
