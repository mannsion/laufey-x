# Input events

A window can deliver native keyboard, mouse, wheel, and cursor enter/leave
events to your runtime. The handlers run before the events reach the page, which
lets the application observe or react to raw input.

```rust
let win = Window::new(800, 600)
  .on_keyboard_event(|e| println!("{} {:?}", e.key, e.modifiers))
  .on_mouse_click(|e| println!("button {} at {},{}", e.button, e.x, e.y))
  .on_mouse_motion(|e| println!("relative {},{}", e.delta_x, e.delta_y))
  .on_wheel(|e| println!("scroll {},{}", e.delta_x, e.delta_y))
  .on_cursor_enter_leave(|e| println!("entered: {}", e.entered))
  .load("index.html");

// Request this from an input handler once the window is focused and the cursor
// is inside it.
win.set_cursor_grab(CursorGrabMode::Locked, |success| {
  println!("cursor-lock request accepted: {success}");
});
```

Keyboard events carry the W3C `key` and `code` strings together with a modifier
bitmask. Mouse events carry the button, the pressed or released state, the
cursor position, the active modifiers, and the click count. Each backend
translates its own native event source — Chromium's event path under CEF,
`NSEvent` on macOS, GDK on Linux, and the Win32 message loop on Windows — into
this common shape, so the same handler works on every backend.

The winit backend supports both native cursor confinement and pointer lock.
`CursorGrabMode::Confined` asks the native system to keep the visible cursor
inside the window while preserving ordinary absolute movement.
`CursorGrabMode::Locked` hides it and makes `on_mouse_motion` receive unbounded,
device-dependent relative deltas. Wayland supplies compositor-generated
unaccelerated deltas rather than raw hardware events. Acquisition requires a
focused window with the cursor inside it and may fail when the native API
rejects the requested mode. The current macOS winit backend supports locked mode
but not confined mode.

The callback reports whether the backend accepted the request. Wayland locked
mode requires both the pointer-constraints and relative-pointer protocols on
winit's display connection. Constraint activation is asynchronous, however, and
winit does not expose the later compositor activation event, so acceptance there
does not prove that the constraint has become active. The backend releases
either mode automatically when the window loses focus or closes.
