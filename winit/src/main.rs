// Copyright 2025 Divy Srivastava. All rights reserved. MIT license.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use laufey_backend_winit_common::{
  define_common_backend_fns, fill_common_api, winit, BackendAccess,
  CommonEvent, CommonState, LaufeyBackendApi, LaufeyJsResultFn,
  LAUFEY_CURSOR_GRAB_CONFINED, LAUFEY_CURSOR_GRAB_LOCKED,
  LAUFEY_CURSOR_GRAB_NONE,
};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{CursorGrabMode as WinitCursorGrabMode, Window};

#[cfg(target_os = "linux")]
mod wayland_cursor_capabilities {
  use std::os::fd::OwnedFd;
  use std::sync::{Arc, Mutex, Weak};

  use super::winit::window::Window;
  use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
  use wayland_client::backend::{self, Backend, ObjectData, ObjectId};
  use wayland_client::protocol::{wl_display, wl_registry};
  use wayland_client::{Connection, Proxy};

  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub(super) enum Capabilities {
    NotWayland,
    Wayland {
      pointer_constraints: bool,
      relative_pointer: bool,
    },
    Unavailable,
  }

  impl Capabilities {
    pub(super) fn supports(self, locked: bool) -> bool {
      match self {
        Self::NotWayland => true,
        Self::Wayland {
          pointer_constraints,
          relative_pointer,
        } => pointer_constraints && (!locked || relative_pointer),
        Self::Unavailable => false,
      }
    }
  }

  struct RegistryData {
    connection: Weak<Connection>,
    capabilities: Arc<Mutex<(bool, bool)>>,
  }

  impl ObjectData for RegistryData {
    fn event(
      self: Arc<Self>,
      _backend: &Backend,
      message: backend::protocol::Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
      let connection = self.connection.upgrade()?;
      let Ok((_registry, event)) =
        wl_registry::WlRegistry::parse_event(&connection, message)
      else {
        return None;
      };
      if let wl_registry::Event::Global { interface, .. } = event {
        let mut capabilities = self.capabilities.lock().unwrap();
        match interface.as_str() {
          "zwp_pointer_constraints_v1" => capabilities.0 = true,
          "zwp_relative_pointer_manager_v1" => capabilities.1 = true,
          _ => {}
        }
      }
      None
    }

    fn destroyed(&self, _object_id: ObjectId) {}
  }

  pub(super) fn query(window: &Window) -> Capabilities {
    let Ok(display_handle) = window.display_handle() else {
      return Capabilities::Unavailable;
    };
    let RawDisplayHandle::Wayland(handle) = display_handle.as_raw() else {
      return Capabilities::NotWayland;
    };

    let backend =
      unsafe { Backend::from_foreign_display(handle.display.as_ptr().cast()) };
    let connection = Arc::new(Connection::from_backend(backend));
    let capabilities = Arc::new(Mutex::new((false, false)));
    let registry_data = Arc::new(RegistryData {
      connection: Arc::downgrade(&connection),
      capabilities: capabilities.clone(),
    });
    let display = connection.display();
    let registry: Result<wl_registry::WlRegistry, _> = display
      .send_constructor(wl_display::Request::GetRegistry {}, registry_data);
    let Ok(_registry) = registry else {
      return Capabilities::Unavailable;
    };
    if connection.roundtrip().is_err() {
      return Capabilities::Unavailable;
    }
    let (pointer_constraints, relative_pointer) = *capabilities.lock().unwrap();
    Capabilities::Wayland {
      pointer_constraints,
      relative_pointer,
    }
  }
}

// --- Backend state ---

static BACKEND_STATE: AtomicPtr<BackendState> =
  AtomicPtr::new(std::ptr::null_mut());

struct BackendState {
  event_proxy: EventLoopProxy<UserEvent>,
  common: CommonState,
}

impl BackendAccess for BackendState {
  type Event = UserEvent;

  fn get() -> Option<&'static Self> {
    let ptr = BACKEND_STATE.load(Ordering::Acquire);
    if ptr.is_null() {
      None
    } else {
      Some(unsafe { &*ptr })
    }
  }

  fn proxy(&self) -> &EventLoopProxy<UserEvent> {
    &self.event_proxy
  }

  fn common(&self) -> &CommonState {
    &self.common
  }

  fn common_event(event: CommonEvent) -> UserEvent {
    UserEvent::Common(event)
  }
}

// Generate all common backend C ABI functions
define_common_backend_fns!(BackendState);

// --- Backend-specific functions ---

unsafe extern "C" fn backend_navigate(
  _data: *mut c_void,
  _window_id: u32,
  _url: *const std::ffi::c_char,
) {
  // no-op — no web engine
}

unsafe extern "C" fn backend_execute_js(
  _data: *mut c_void,
  _window_id: u32,
  _script: *const std::ffi::c_char,
  _callback: Option<LaufeyJsResultFn>,
  _callback_data: *mut c_void,
) {
  // no-op — no web engine
}

// --- API construction ---

fn create_backend_api() -> LaufeyBackendApi {
  let mut api = laufey_backend_winit_common::create_api_base();
  fill_common_api!(api);
  api.navigate = Some(backend_navigate);
  api.execute_js = Some(backend_execute_js);
  api
}

// --- Event loop ---

#[derive(Debug)]
enum UserEvent {
  Common(CommonEvent),
}

struct WindowInfo {
  window: Window,
  modifiers: ModifiersState,
  cursor_inside: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcceptedCursorGrabMode {
  Confined,
  Locked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AcceptedCursorGrab {
  window_id: u32,
  mode: AcceptedCursorGrabMode,
}

struct PendingPointerUngrab {
  window_id: u32,
  // Keep a closing native window alive until its process-wide grab releases.
  detached_window: Option<Window>,
}

fn accepted_cursor_grab_mode(mode: i32) -> Option<AcceptedCursorGrabMode> {
  match mode {
    LAUFEY_CURSOR_GRAB_CONFINED => Some(AcceptedCursorGrabMode::Confined),
    LAUFEY_CURSOR_GRAB_LOCKED => Some(AcceptedCursorGrabMode::Locked),
    _ => None,
  }
}

fn locked_cursor_grab_window(grab: Option<AcceptedCursorGrab>) -> Option<u32> {
  match grab {
    Some(AcceptedCursorGrab {
      window_id,
      mode: AcceptedCursorGrabMode::Locked,
    }) => Some(window_id),
    _ => None,
  }
}

fn cursor_release_complete(
  grab: Option<AcceptedCursorGrab>,
  pending_ungrab: Option<u32>,
  window_id: u32,
) -> bool {
  grab.map(|grab| grab.window_id) != Some(window_id)
    && pending_ungrab != Some(window_id)
}

#[cfg(target_os = "windows")]
fn release_process_cursor_grab() -> bool {
  unsafe {
    windows_sys::Win32::UI::WindowsAndMessaging::ClipCursor(std::ptr::null())
      != 0
  }
}

#[cfg(target_os = "macos")]
fn release_process_cursor_grab() -> bool {
  core_graphics::display::CGDisplay::associate_mouse_and_mouse_cursor_position(
    true,
  )
  .is_ok()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn release_process_cursor_grab() -> bool {
  true
}

struct App {
  // Map from our window_id to winit WindowId + Window
  windows: HashMap<u32, WindowInfo>,
  // Reverse map from winit WindowId to our window_id
  winit_to_laufey: HashMap<winit::window::WindowId, u32>,
  // Most recently focused LAUFEY window (for app-scoped dock ops on Windows/Linux).
  focused_laufey_id: Option<u32>,
  // At most one native cursor-grab request may be accepted at a time. On
  // Wayland, protocol activation happens asynchronously and winit does not
  // expose the activation event.
  cursor_grab: Option<AcceptedCursorGrab>,
  // A live window whose OS grab failed to release and must be retried.
  pending_pointer_ungrab: Option<PendingPointerUngrab>,
  // A process-wide grab whose native window was already destroyed.
  pending_process_ungrab: bool,
  pending_mouse_motion: (f64, f64),
  #[cfg(target_os = "linux")]
  wayland_cursor_capabilities:
    Option<wayland_cursor_capabilities::Capabilities>,
}

impl App {
  fn new() -> Self {
    Self {
      windows: HashMap::new(),
      winit_to_laufey: HashMap::new(),
      focused_laufey_id: None,
      cursor_grab: None,
      pending_pointer_ungrab: None,
      pending_process_ungrab: false,
      pending_mouse_motion: (0.0, 0.0),
      #[cfg(target_os = "linux")]
      wayland_cursor_capabilities: None,
    }
  }

  fn focused_window(&self) -> Option<(&Window, u32)> {
    let id = self
      .focused_laufey_id
      .or_else(|| self.windows.keys().next().copied())?;
    self.windows.get(&id).map(|info| (&info.window, id))
  }

  fn create_window(
    &mut self,
    event_loop: &winit::event_loop::ActiveEventLoop,
    window_id: u32,
  ) {
    let state = BackendState::get().expect("BackendState not initialized");
    let attrs = state
      .common
      .with_window(window_id, |ws| {
        laufey_backend_winit_common::apply_pending_attrs(
          ws,
          Window::default_attributes(),
        )
      })
      .unwrap_or_else(Window::default_attributes);

    let window = event_loop
      .create_window(attrs)
      .expect("Failed to create winit Window");

    state.common.with_window(window_id, |ws| {
      laufey_backend_winit_common::apply_pending_post_create(ws, &window);
    });
    laufey_backend_winit_common::store_window_handles(window_id, &window);

    let winit_id = window.id();
    self.winit_to_laufey.insert(winit_id, window_id);
    self.windows.insert(
      window_id,
      WindowInfo {
        window,
        modifiers: ModifiersState::default(),
        cursor_inside: false,
      },
    );
  }

  fn close_window(&mut self, window_id: u32, native_destroyed: bool) {
    let destroyed_grab = native_destroyed
      && (self.cursor_grab.map(|grab| grab.window_id) == Some(window_id)
        || self
          .pending_pointer_ungrab
          .as_ref()
          .is_some_and(|pending| pending.window_id == window_id));
    if native_destroyed {
      if self.cursor_grab.map(|grab| grab.window_id) == Some(window_id) {
        self.cursor_grab = None;
        self.pending_mouse_motion = (0.0, 0.0);
      }
      if self
        .pending_pointer_ungrab
        .as_ref()
        .is_some_and(|pending| pending.window_id == window_id)
      {
        // The native handle can no longer be used for another release attempt.
        self.pending_pointer_ungrab = None;
      }
    } else {
      self.force_release_cursor_grab(window_id);
    }
    if let Some(info) = self.windows.remove(&window_id) {
      self.winit_to_laufey.remove(&info.window.id());
      laufey_backend_winit_common::remove_window_handles(window_id);
      if let Some(state) = BackendState::get() {
        state.common.remove_window(window_id);
      }
      if !native_destroyed
        && self
          .pending_pointer_ungrab
          .as_ref()
          .is_some_and(|pending| pending.window_id == window_id)
      {
        info.window.set_visible(false);
        self
          .pending_pointer_ungrab
          .as_mut()
          .unwrap()
          .detached_window = Some(info.window);
      }
    }
    if destroyed_grab {
      for info in self.windows.values() {
        info.window.set_cursor_visible(true);
      }
      self.pending_process_ungrab = !release_process_cursor_grab();
    }
  }

  fn laufey_id(&self, winit_id: winit::window::WindowId) -> Option<u32> {
    self.winit_to_laufey.get(&winit_id).copied()
  }

  fn supports_cursor_grab_mode(
    &mut self,
    window_id: u32,
    mode: AcceptedCursorGrabMode,
  ) -> bool {
    #[cfg(target_os = "linux")]
    {
      let capabilities =
        if let Some(capabilities) = self.wayland_cursor_capabilities {
          capabilities
        } else {
          let Some(info) = self.windows.get(&window_id) else {
            return false;
          };
          let capabilities = wayland_cursor_capabilities::query(&info.window);
          self.wayland_cursor_capabilities = Some(capabilities);
          capabilities
        };
      capabilities.supports(mode == AcceptedCursorGrabMode::Locked)
    }

    #[cfg(not(target_os = "linux"))]
    {
      let _ = (window_id, mode);
      true
    }
  }

  fn set_cursor_grab(&mut self, window_id: u32, mode: i32) -> bool {
    if mode == LAUFEY_CURSOR_GRAB_NONE {
      self.pending_mouse_motion = (0.0, 0.0);
      self.retry_process_ungrab();
      if self.pending_process_ungrab {
        return false;
      }
      if self
        .pending_pointer_ungrab
        .as_ref()
        .is_some_and(|pending| pending.window_id == window_id)
      {
        self.retry_pointer_ungrab();
        if self
          .pending_pointer_ungrab
          .as_ref()
          .is_some_and(|pending| pending.window_id == window_id)
        {
          return false;
        }
      }
      if cursor_release_complete(
        self.cursor_grab,
        self
          .pending_pointer_ungrab
          .as_ref()
          .map(|pending| pending.window_id),
        window_id,
      ) {
        return self.windows.contains_key(&window_id);
      }
      let Some(info) = self.windows.get(&window_id) else {
        self.cursor_grab = None;
        return false;
      };
      let released = info
        .window
        .set_cursor_grab(WinitCursorGrabMode::None)
        .is_ok();
      info.window.set_cursor_visible(true);
      if released {
        self.pending_pointer_ungrab = None;
      } else {
        self.pending_pointer_ungrab = Some(PendingPointerUngrab {
          window_id,
          detached_window: None,
        });
      }
      self.cursor_grab = None;
      return released;
    }

    let Some(requested_mode) = accepted_cursor_grab_mode(mode) else {
      return false;
    };

    self.retry_process_ungrab();
    self.retry_pointer_ungrab();
    if self.pending_process_ungrab || self.pending_pointer_ungrab.is_some() {
      return false;
    }
    if self.focused_laufey_id != Some(window_id)
      || !self
        .windows
        .get(&window_id)
        .is_some_and(|info| info.cursor_inside)
    {
      return false;
    }
    if !self.supports_cursor_grab_mode(window_id, requested_mode) {
      return false;
    }
    if self.cursor_grab
      == Some(AcceptedCursorGrab {
        window_id,
        mode: requested_mode,
      })
    {
      return true;
    }
    if let Some(previous) = self.cursor_grab {
      if !self.set_cursor_grab(previous.window_id, LAUFEY_CURSOR_GRAB_NONE) {
        return false;
      }
    }

    let Some(info) = self.windows.get(&window_id) else {
      return false;
    };
    let accepted = match requested_mode {
      AcceptedCursorGrabMode::Confined => info
        .window
        .set_cursor_grab(WinitCursorGrabMode::Confined)
        .is_ok(),
      AcceptedCursorGrabMode::Locked => {
        // X11 does not implement Locked, but Confined plus XI2 raw motion
        // provides the same logical locked-pointer semantics to the runtime.
        info
          .window
          .set_cursor_grab(WinitCursorGrabMode::Locked)
          .or_else(|_| {
            info.window.set_cursor_grab(WinitCursorGrabMode::Confined)
          })
          .is_ok()
      }
    };
    if accepted {
      info
        .window
        .set_cursor_visible(requested_mode == AcceptedCursorGrabMode::Confined);
      self.cursor_grab = Some(AcceptedCursorGrab {
        window_id,
        mode: requested_mode,
      });
      self.pending_pointer_ungrab = None;
      self.pending_mouse_motion = (0.0, 0.0);
    }
    accepted
  }

  fn force_release_cursor_grab(&mut self, window_id: u32) {
    if self.cursor_grab.map(|grab| grab.window_id) != Some(window_id) {
      if self
        .pending_pointer_ungrab
        .as_ref()
        .is_some_and(|pending| pending.window_id == window_id)
      {
        self.retry_pointer_ungrab();
      }
      self.pending_mouse_motion = (0.0, 0.0);
      return;
    }
    if let Some(info) = self.windows.get(&window_id) {
      let released = info
        .window
        .set_cursor_grab(WinitCursorGrabMode::None)
        .is_ok();
      info.window.set_cursor_visible(true);
      if released {
        self.pending_pointer_ungrab = None;
      } else {
        self.pending_pointer_ungrab = Some(PendingPointerUngrab {
          window_id,
          detached_window: None,
        });
      }
    }
    self.cursor_grab = None;
    self.pending_mouse_motion = (0.0, 0.0);
  }

  fn retry_pointer_ungrab(&mut self) {
    let Some(pending) = self.pending_pointer_ungrab.as_ref() else {
      return;
    };
    let window = pending.detached_window.as_ref().or_else(|| {
      self
        .windows
        .get(&pending.window_id)
        .map(|info| &info.window)
    });
    let Some(window) = window else {
      self.pending_pointer_ungrab = None;
      return;
    };
    let released = window.set_cursor_grab(WinitCursorGrabMode::None).is_ok();
    window.set_cursor_visible(true);
    if released {
      self.pending_pointer_ungrab = None;
    }
  }

  fn retry_process_ungrab(&mut self) {
    if self.pending_process_ungrab && release_process_cursor_grab() {
      self.pending_process_ungrab = false;
    }
  }

  fn flush_mouse_motion(&mut self) {
    let Some(window_id) = locked_cursor_grab_window(self.cursor_grab) else {
      self.pending_mouse_motion = (0.0, 0.0);
      return;
    };
    let delta = std::mem::replace(&mut self.pending_mouse_motion, (0.0, 0.0));
    if delta == (0.0, 0.0) {
      return;
    }
    let Some(state) = BackendState::get() else {
      return;
    };
    let modifiers = self
      .windows
      .get(&window_id)
      .map(|info| info.modifiers)
      .unwrap_or_default();
    laufey_backend_winit_common::dispatch_mouse_motion_event(
      &state.common.handlers,
      window_id,
      delta.0,
      delta.1,
      modifiers,
    );
  }
}

impl ApplicationHandler<UserEvent> for App {
  fn resumed(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
    // Windows are created on-demand via CreateWindow events
  }

  fn user_event(
    &mut self,
    event_loop: &winit::event_loop::ActiveEventLoop,
    event: UserEvent,
  ) {
    self.flush_mouse_motion();
    match event {
      UserEvent::Common(ref common) => match common {
        CommonEvent::Quit => {
          event_loop.exit();
        }
        CommonEvent::CreateWindow { window_id } => {
          self.create_window(event_loop, *window_id);
        }
        CommonEvent::CloseWindow { window_id } => {
          self.close_window(*window_id, false);
          if self.windows.is_empty() {
            event_loop.exit();
          }
        }
        CommonEvent::SetCursorGrab {
          window_id,
          mode,
          completion,
        } => {
          let success = self.set_cursor_grab(*window_id, *mode);
          completion.complete(success);
        }
        CommonEvent::UiTask { task, data } => {
          unsafe { task(*data as *mut c_void) };
        }
        CommonEvent::DockTask => {
          laufey_backend_winit_common::dock::drain_and_apply(
            self.focused_window(),
          );
        }
        CommonEvent::TrayTask => {
          laufey_backend_winit_common::tray::drain_and_apply();
        }
        other => {
          let wid = match other {
            CommonEvent::SetTitle { window_id }
            | CommonEvent::SetWindowSize { window_id }
            | CommonEvent::SetWindowPosition { window_id }
            | CommonEvent::SetResizable { window_id }
            | CommonEvent::SetAlwaysOnTop { window_id }
            | CommonEvent::SetClickPassthrough { window_id }
            | CommonEvent::Show { window_id }
            | CommonEvent::Hide { window_id }
            | CommonEvent::Focus { window_id }
            | CommonEvent::SetApplicationMenu { window_id }
            | CommonEvent::ShowContextMenu { window_id } => *window_id,
            _ => return,
          };
          if let Some(info) = self.windows.get(&wid) {
            laufey_backend_winit_common::handle_common_event::<BackendState>(
              common,
              wid,
              &info.window,
            );
          }
        }
      },
    }
  }

  fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
    self.retry_process_ungrab();
    self.retry_pointer_ungrab();
    if self.pending_process_ungrab || self.pending_pointer_ungrab.is_some() {
      event_loop.set_control_flow(ControlFlow::WaitUntil(
        Instant::now() + Duration::from_millis(50),
      ));
    } else {
      event_loop.set_control_flow(ControlFlow::Wait);
    }
    self.flush_mouse_motion();
    laufey_backend_winit_common::poll_menu_events();
    // The tray lives on the primary monitor (menu bar / taskbar); its scale
    // factor converts tray-icon's physical rect into the logical window space.
    let scale_factor = event_loop
      .primary_monitor()
      .map(|m| m.scale_factor())
      .unwrap_or(1.0);
    laufey_backend_winit_common::tray::poll_tray_events(scale_factor);
  }

  fn suspended(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
    if let Some(grab) = self.cursor_grab {
      self.force_release_cursor_grab(grab.window_id);
    }
  }

  fn device_event(
    &mut self,
    _event_loop: &winit::event_loop::ActiveEventLoop,
    _device_id: DeviceId,
    event: DeviceEvent,
  ) {
    let Some(window_id) = locked_cursor_grab_window(self.cursor_grab) else {
      return;
    };
    if self.focused_laufey_id != Some(window_id) {
      return;
    }
    let DeviceEvent::MouseMotion { delta } = event else {
      return;
    };
    let Some(info) = self.windows.get(&window_id) else {
      return;
    };
    if !info.cursor_inside {
      return;
    }
    self.pending_mouse_motion.0 += delta.0;
    self.pending_mouse_motion.1 += delta.1;
  }

  fn window_event(
    &mut self,
    event_loop: &winit::event_loop::ActiveEventLoop,
    winit_window_id: winit::window::WindowId,
    event: WindowEvent,
  ) {
    let laufey_id = match self.laufey_id(winit_window_id) {
      Some(id) => id,
      None => return,
    };

    let state = match BackendState::get() {
      Some(s) => s,
      None => return,
    };

    self.flush_mouse_motion();
    let modifiers = match self.windows.get(&laufey_id) {
      Some(info) => info.modifiers,
      None => return,
    };

    match event {
      WindowEvent::CloseRequested => {
        let proceed =
          laufey_backend_winit_common::dispatch_close_requested_event(
            &state.common.handlers,
            laufey_id,
          );
        if proceed {
          self.close_window(laufey_id);
          if self.windows.is_empty() {
            event_loop.exit();
          }
        }
      }
      WindowEvent::Resized(PhysicalSize { width, height }) => {
        state.common.with_window(laufey_id, |ws| {
          *ws.current_size.lock().unwrap() =
            Some((width as i32, height as i32));
        });
        laufey_backend_winit_common::dispatch_resize_event(
          &state.common.handlers,
          laufey_id,
          width as i32,
          height as i32,
        );
      }
      WindowEvent::Moved(PhysicalPosition { x, y }) => {
        state.common.with_window(laufey_id, |ws| {
          *ws.pending_position.lock().unwrap() = Some((x, y));
        });
        laufey_backend_winit_common::dispatch_move_event(
          &state.common.handlers,
          laufey_id,
          x,
          y,
        );
      }
      WindowEvent::ModifiersChanged(new_modifiers) => {
        if let Some(info) = self.windows.get_mut(&laufey_id) {
          info.modifiers = new_modifiers.state();
        }
      }
      WindowEvent::KeyboardInput {
        event: ref key_event,
        ..
      } => {
        laufey_backend_winit_common::dispatch_keyboard_event(
          &state.common.handlers,
          laufey_id,
          key_event,
          modifiers,
        );
      }
      WindowEvent::CursorMoved { position, .. } => {
        if let Some(info) = self.windows.get_mut(&laufey_id) {
          info.cursor_inside = true;
        }
        if locked_cursor_grab_window(self.cursor_grab) != Some(laufey_id) {
          state.common.with_window(laufey_id, |ws| {
            *ws.cursor_position.lock().unwrap() = (position.x, position.y);
          });
          laufey_backend_winit_common::dispatch_mouse_move_event(
            &state.common.handlers,
            laufey_id,
            position.x,
            position.y,
            modifiers,
          );
        }
      }
      WindowEvent::MouseInput {
        state: button_state,
        button,
        ..
      } => {
        if let Some(info) = self.windows.get_mut(&laufey_id) {
          info.cursor_inside = true;
        }
        state.common.with_window(laufey_id, |ws| {
          laufey_backend_winit_common::dispatch_mouse_click_event(
            &state.common.handlers,
            ws,
            laufey_id,
            button_state,
            button,
            modifiers,
          );
        });
      }
      WindowEvent::MouseWheel { delta, .. } => {
        state.common.with_window(laufey_id, |ws| {
          laufey_backend_winit_common::dispatch_wheel_event(
            &state.common.handlers,
            ws,
            laufey_id,
            delta,
            modifiers,
          );
        });
      }
      WindowEvent::CursorEntered { .. } => {
        if let Some(info) = self.windows.get_mut(&laufey_id) {
          info.cursor_inside = true;
        }
        state.common.with_window(laufey_id, |ws| {
          laufey_backend_winit_common::dispatch_cursor_enter_leave_event(
            &state.common.handlers,
            ws,
            laufey_id,
            true,
            modifiers,
          );
        });
      }
      WindowEvent::CursorLeft { .. } => {
        if let Some(info) = self.windows.get_mut(&laufey_id) {
          info.cursor_inside = false;
        }
        self.force_release_cursor_grab(laufey_id);
        state.common.with_window(laufey_id, |ws| {
          laufey_backend_winit_common::dispatch_cursor_enter_leave_event(
            &state.common.handlers,
            ws,
            laufey_id,
            false,
            modifiers,
          );
        });
      }
      WindowEvent::Focused(focused) => {
        if focused {
          self.focused_laufey_id = Some(laufey_id);
        } else {
          if self.cursor_grab.map(|grab| grab.window_id) == Some(laufey_id) {
            self.force_release_cursor_grab(laufey_id);
          }
          if self.focused_laufey_id == Some(laufey_id) {
            self.focused_laufey_id = None;
          }
        }
        laufey_backend_winit_common::dispatch_focused_event(
          &state.common.handlers,
          laufey_id,
          focused,
        );
      }

      WindowEvent::ThemeChanged(_) => {}
      WindowEvent::Destroyed => {
        // Normal close paths remove the mapping before winit emits Destroyed.
        // If the OS destroys the window directly, still notify the runtime so
        // its per-window handlers and state are released.
        laufey_backend_winit_common::dispatch_close_requested_event(
          &state.common.handlers,
          laufey_id,
        );
        self.close_window(laufey_id, true);
        if self.windows.is_empty() {
          event_loop.exit();
        }
      }
      WindowEvent::DroppedFile(_) => {}
      WindowEvent::HoveredFile(_) => {}
      WindowEvent::HoveredFileCancelled => {}
      WindowEvent::Ime(_) => {}

      WindowEvent::Touch(_)
      | WindowEvent::PinchGesture { .. }
      | WindowEvent::PanGesture { .. }
      | WindowEvent::DoubleTapGesture { .. }
      | WindowEvent::RotationGesture { .. }
      | WindowEvent::TouchpadPressure { .. } => {
        // TODO: touch
      }
      WindowEvent::ActivationTokenDone { .. }
      | WindowEvent::AxisMotion { .. }
      | WindowEvent::ScaleFactorChanged { .. }
      | WindowEvent::Occluded(_)
      | WindowEvent::RedrawRequested => {
        // wont implement
      }
    }
  }
}

fn main() -> Result<(), Box<dyn Error>> {
  let event_loop = EventLoop::with_user_event()
    .build()
    .expect("Failed to create EventLoop");

  let backend_state = Box::new(BackendState {
    event_proxy: event_loop.create_proxy(),
    common: CommonState::new(),
  });
  BACKEND_STATE.store(Box::into_raw(backend_state), Ordering::Release);

  let runtime_status =
    laufey_backend_winit_common::load_and_start_runtime(create_backend_api());

  let mut app = App::new();
  event_loop.run_app(&mut app)?;
  let status = runtime_status.load(Ordering::Acquire);
  if status != 0 {
    return Err(format!("Laufey runtime failed with code {status}").into());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cursor_grab_modes_reject_unknown_values() {
    assert_eq!(
      accepted_cursor_grab_mode(LAUFEY_CURSOR_GRAB_CONFINED),
      Some(AcceptedCursorGrabMode::Confined)
    );
    assert_eq!(
      accepted_cursor_grab_mode(LAUFEY_CURSOR_GRAB_LOCKED),
      Some(AcceptedCursorGrabMode::Locked)
    );
    assert_eq!(accepted_cursor_grab_mode(LAUFEY_CURSOR_GRAB_NONE), None);
    assert_eq!(accepted_cursor_grab_mode(99), None);
  }

  #[test]
  fn only_locked_grab_receives_raw_motion() {
    let confined = Some(AcceptedCursorGrab {
      window_id: 7,
      mode: AcceptedCursorGrabMode::Confined,
    });
    let locked = Some(AcceptedCursorGrab {
      window_id: 7,
      mode: AcceptedCursorGrabMode::Locked,
    });
    assert_eq!(locked_cursor_grab_window(confined), None);
    assert_eq!(locked_cursor_grab_window(locked), Some(7));
  }

  #[test]
  fn pending_native_ungrab_is_not_a_completed_release() {
    assert!(cursor_release_complete(None, None, 7));
    assert!(!cursor_release_complete(None, Some(7), 7));
    assert!(cursor_release_complete(None, Some(8), 7));

    let locked = Some(AcceptedCursorGrab {
      window_id: 7,
      mode: AcceptedCursorGrabMode::Locked,
    });
    assert!(!cursor_release_complete(locked, None, 7));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn wayland_locked_mode_requires_both_protocols() {
    use wayland_cursor_capabilities::Capabilities;

    assert!(Capabilities::NotWayland.supports(true));
    assert!(Capabilities::Wayland {
      pointer_constraints: true,
      relative_pointer: false,
    }
    .supports(false));
    assert!(!Capabilities::Wayland {
      pointer_constraints: true,
      relative_pointer: false,
    }
    .supports(true));
    assert!(Capabilities::Wayland {
      pointer_constraints: true,
      relative_pointer: true,
    }
    .supports(true));
  }
}
