# Daemon TUI Architecture Guide

Reference document for the swarm daemon TUI (`daemon_tui/`). Covers patterns,
pitfalls, and the rationale behind architectural decisions.

## 1. Event Loop: `EventStream` + `tokio::select!`

### Problem

`crossterm::event::poll(Duration)` is a **blocking** call. In a tokio runtime,
calling it from an async context blocks the entire thread for up to the timeout
duration. This starves channel receivers, timers, and any other async work
sharing that thread.

### Solution

Use `crossterm::event::EventStream` (requires feature `event-stream`):

```rust
use crossterm::event::EventStream;
use futures::StreamExt;

let mut events = EventStream::new();

loop {
    tokio::select! {
        Some(Ok(event)) = events.next() => { /* handle terminal event */ }
        Some(msg) = channel_rx.recv() => { /* handle daemon message */ }
        _ = tick.tick() => { /* animation / periodic tasks */ }
    }
}
```

`EventStream` internally uses a background thread to poll crossterm, then
exposes results as a `futures::Stream`. The `.next()` call is non-blocking
and integrates cleanly with `tokio::select!`.

### Why `tokio::select!` not `futures::select!`

- `tokio::select!` does **not** require `.fuse()` on futures.
- Branches can use `&mut` references naturally.
- More ergonomic for the common case of selecting over channels + streams.

## 2. Daemon IPC: Dedicated Reader Task

### Problem

`BufReader::read_line` is **not cancellation-safe** in `tokio::select!`. If a
timeout or competing branch fires while `read_line` has partially consumed data
from the internal buffer into the output `String`, that data is lost when the
future is dropped.

### Solution

Spawn a dedicated reader task that owns the read half of the socket:

```rust
async fn daemon_reader_task(
    mut reader: DaemonReader,
    tx: mpsc::UnboundedSender<Result<DaemonResponse>>,
) {
    loop {
        let result = reader.next_response().await;
        let is_err = result.is_err();
        if tx.send(result).is_err() { break; }
        if is_err { break; }
    }
}
```

The reader task runs `read_line` in a tight loop with **no** `select!` or
timeouts. It just reads line after line, parses each as JSON, and sends the
result on an `mpsc::unbounded_channel`. The main loop receives from this
channel, which **is** cancellation-safe.

### Connection Lifecycle

1. `DaemonClient::connect()` returns a client with both reader and writer.
2. `client.subscribe()` uses the internal reader to get the OK response.
3. `client.take_reader()` extracts the reader half as a `DaemonReader`.
4. Spawn `daemon_reader_task(reader, tx)`.
5. Main loop sends with `client.send()`, receives from `rx`.

On reconnect: create a new client, subscribe, take reader, spawn a new task.
The old task dies naturally when its socket closes.

### Request/Response Model

The daemon uses a **subscription** model, not request/response correlation:

- After `Subscribe`, all events stream to the client.
- Requests like `ListWorkers` or `GetHistory` produce responses that arrive
  in-band with events (no message IDs needed).
- `pending_history: VecDeque<String>` tracks which worker each `GetHistory`
  response belongs to (FIFO ordering).

## 3. Async Data Loading

### Principle: Never Block the Render Thread

| Operation | Strategy |
|-----------|----------|
| `detect_repos()` (git subprocesses) | `spawn_blocking` |
| Worker list | Fire-and-forget `ListWorkers`, response arrives via channel |
| Worker history | Lazy-load on first select, response arrives via channel |
| Reconnection | Attempt in tick handler, with short timeouts |

### Lazy History Loading

Instead of fetching history for all workers at startup (O(n) sequential round
trips), load history on-demand when a worker is first selected:

```rust
if !app.history_loaded.contains(&worker_id) {
    app.history_loaded.insert(worker_id.clone());
    app.pending_history.push_back(worker_id.clone());
    client.send(&GetHistory { worktree_id: worker_id }).await;
}
```

Response arrives through the daemon channel and is matched to the worker via
`pending_history.pop_front()`.

## 4. Rendering

### Dirty-Flag Pattern

Only call `terminal.draw()` when state has changed:

```rust
if app.needs_redraw {
    terminal.draw(|frame| render::draw(frame, app))?;
    app.needs_redraw = false;
}
```

Set `needs_redraw = true` when:
- Terminal event received (key press, mouse)
- Daemon response received
- Tick fires (for spinner animation)
- State mutation methods (`set_status`, `update_worker_list`, etc.)

### Tick Rate

250ms tick interval (4 Hz) is sufficient for text TUI animation (spinners).
User interactions and daemon events trigger immediate redraws outside the tick.

Use `MissedTickBehavior::Skip` to avoid burst redraws after a slow handler.

## 5. Testing

### Unit Tests (no terminal needed)

Test pure state logic by calling `handle_key()`, `handle_daemon_response()`,
and asserting on `DaemonTuiApp` state:

```rust
let mut app = app_with_workers(&["w-1"]);
handle_key(&mut app, key(KeyCode::Char('j')));
assert_eq!(app.selected, 1);
```

### Integration Tests

Test the socket protocol with a minimal server (no swarm imports — binary crate
can't be imported). Use raw JSON strings:

```rust
let json = r#"{"action":"ping"}"#;
writer.write_all(json.as_bytes()).await;
let resp = reader.read_line(&mut buf).await;
assert!(buf.contains("ok"));
```

### Visual Tests (future)

Ratatui's `TestBackend` + `Buffer` assertions for render output.
`insta` snapshots for regression testing of complex layouts.

## 6. Architecture Diagram

```
Terminal Events          Daemon Socket
      |                       |
 EventStream             DaemonReader (background task)
      |                       |
      v                       v
   tokio::select! <--- mpsc::channel
      |
      v
  handle_key()          handle_daemon_response()
      |                       |
      v                       v
  KeyAction enum        App state mutation
      |                       |
      v                       v
  client.send()         needs_redraw = true
      |                       |
      +--------> draw() <----+
```
