use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use color_eyre::owo_colors::OwoColorize;
use itertools::Itertools;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use seelen_core::handlers::SeelenEvent;
use serde::Serialize;
use tauri::Emitter;
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, TranslateMessage, EVENT_MAX, EVENT_MIN, MSG,
        },
    },
};

use crate::{
    error_handler::Result,
    log_error,
    modules::{
        input::{domain::Point, Mouse},
        virtual_desk::{get_vd_manager, VirtualDesktopEvent, VirtualDesktopManager},
    },
    seelen::{get_app_handle, Seelen, SEELEN},
    seelen_weg::SeelenWeg,
    seelen_wm_v2::instance::WindowManagerV2,
    state::{application::FULL_STATE, domain::AppExtraFlag},
    trace_lock,
    utils::{constants::IGNORE_FOCUS, spawn_named_thread},
    windows_api::{window::Window, WindowsApi},
    winevent::WinEvent,
};

lazy_static! {
    static ref HOOK_MANAGER: Arc<Mutex<HookManager>> = Arc::new(Mutex::new(HookManager::new()));
    // Last active window omitting all the seelen overlays
    pub static ref LAST_ACTIVE_NOT_SEELEN: AtomicIsize = AtomicIsize::new(WindowsApi::get_foreground_window().0 as _);
}

pub static LOG_WIN_EVENTS: AtomicBool = AtomicBool::new(false);

pub struct HookManager {
    skip: HashMap<isize, Vec<WinEvent>>,
}

#[derive(Serialize, Clone)]
pub struct FocusedApp {
    hwnd: isize,
    title: String,
    name: String,
    exe: Option<PathBuf>,
}

impl HookManager {
    fn new() -> Self {
        Self {
            skip: HashMap::new(),
        }
    }

    pub fn run_with_async<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce(&mut HookManager) -> T,
        F: Send + 'static,
        T: Send + 'static,
    {
        std::thread::spawn(move || f(&mut *trace_lock!(HOOK_MANAGER)))
    }

    pub fn skip(&mut self, event: WinEvent, hwnd: HWND) {
        self.skip.entry(hwnd.0 as _).or_default().push(event)
    }

    fn should_skip(&self, event: WinEvent, hwnd: HWND) -> bool {
        if let Some(v) = self.skip.get(&(hwnd.0 as _)) {
            return v.contains(&event);
        }
        false
    }

    fn skip_done(&mut self, event: WinEvent, hwnd: HWND) {
        if LOG_WIN_EVENTS.load(Ordering::Relaxed) {
            log::debug!("Skipping WinEvent::{:?}", event);
        }

        let hwnd = hwnd.0 as isize;
        if let Some(v) = self.skip.get_mut(&hwnd) {
            if let Some(pos) = v.iter().position(|e| e == &event) {
                v.remove(pos);
            }
            if v.is_empty() {
                self.skip.remove(&hwnd);
            }
        }
    }

    fn log_event(event: WinEvent, origin: HWND) {
        if !LOG_WIN_EVENTS.load(Ordering::Relaxed) || event == WinEvent::ObjectLocationChange {
            return;
        }

        log::debug!(
            "{:?}({:?}) || {} || {} || {}",
            event.green(),
            origin.0,
            WindowsApi::exe(origin).unwrap_or_default(),
            WindowsApi::get_class(origin).unwrap_or_default(),
            WindowsApi::get_window_text(origin),
        );
    }

    fn event(&mut self, event: WinEvent, origin: HWND, seelen: &mut Seelen) {
        Self::log_event(event, origin);

        if self.should_skip(event, origin) {
            self.skip_done(event, origin);
            return;
        }

        let window = Window::from(origin);
        if event == WinEvent::SystemForeground && !window.is_seelen_overlay() {
            LAST_ACTIVE_NOT_SEELEN.store(origin.0 as _, Ordering::Relaxed);
        }

        if event == WinEvent::ObjectFocus || event == WinEvent::SystemForeground {
            let title = window.title();
            if IGNORE_FOCUS.contains(&title) {
                if LOG_WIN_EVENTS.load(Ordering::Relaxed) {
                    log::trace!("Skipping WinEvent::{:?}", event);
                }
                return;
            }
            log_error!(get_app_handle().emit(
                SeelenEvent::GlobalFocusChanged,
                FocusedApp {
                    title,
                    hwnd: origin.0 as _,
                    name: window
                        .app_display_name()
                        .unwrap_or(String::from("Error on App Name")),
                    exe: window.exe().ok(),
                },
            ));
        }

        if let VirtualDesktopManager::Seelen(vd) = get_vd_manager().as_ref() {
            log_error!(vd.on_win_event(event, &window));
        }

        if seelen.state().is_weg_enabled() {
            std::thread::spawn(move || {
                log_error!(SeelenWeg::process_global_win_event(event, &window));
            });
        }

        if seelen.state().is_window_manager_enabled() {
            std::thread::spawn(move || {
                log_error!(WindowManagerV2::process_win_event(event, &window));
            });
        }

        for monitor in seelen.monitors_mut() {
            if let Some(toolbar) = monitor.toolbar_mut() {
                log_error!(toolbar.process_win_event(event, origin));
            }

            if let Some(weg) = monitor.weg_mut() {
                log_error!(weg.process_individual_win_event(event, origin));
            }
        }
    }
}

pub fn process_vd_event(event: VirtualDesktopEvent) -> Result<()> {
    if FULL_STATE.load().is_window_manager_enabled() {
        log_error!(WindowManagerV2::process_vd_event(&event));
    }

    match event {
        VirtualDesktopEvent::DesktopCreated(_)
        | VirtualDesktopEvent::DesktopDestroyed {
            destroyed: _,
            fallback: _,
        }
        | VirtualDesktopEvent::DesktopMoved {
            desktop: _,
            old_index: _,
            new_index: _,
        }
        | VirtualDesktopEvent::DesktopNameChanged(_, _) => {
            let desktops = get_vd_manager()
                .get_all()?
                .iter()
                .map(|d| d.as_serializable())
                .collect_vec();
            get_app_handle().emit(SeelenEvent::WorkspacesChanged, &desktops)?;
        }

        VirtualDesktopEvent::DesktopChanged { new, old: _ } => {
            get_app_handle().emit(SeelenEvent::ActiveWorkspaceChanged, new.id())?;
        }
        VirtualDesktopEvent::WindowChanged(window) => {
            let hwnd = HWND(window as _);
            if WindowsApi::is_window(hwnd) {
                if let Some(config) = FULL_STATE.load().get_app_config_by_window(hwnd) {
                    let vd = get_vd_manager();
                    if config.options.contains(&AppExtraFlag::Pinned)
                        && !vd.is_pinned_window(window)?
                    {
                        vd.pin_window(window)?;
                    }
                }
            }
        }
        _ => {}
    }

    Ok(())
}

lazy_static! {
    static ref DICT: Arc<Mutex<HashMap<isize, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
}
static LAST_LOCATION_CHANGED: AtomicIsize = AtomicIsize::new(0);

pub fn location_delay_completed(origin: HWND) -> bool {
    let last = LAST_LOCATION_CHANGED.load(Ordering::Acquire);
    let mut dict = trace_lock!(DICT);

    let should_continue = match dict.entry(origin.0 as _) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if last != origin.0 as isize || entry.get().elapsed() > Duration::from_millis(50) {
                entry.insert(Instant::now());
                true
            } else {
                false
            }
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Instant::now());
            true
        }
    };

    if should_continue {
        LAST_LOCATION_CHANGED.store(origin.0 as _, Ordering::Release);
    }

    should_continue
}

pub extern "system" fn win_event_hook(
    hook_handle: HWINEVENTHOOK,
    event: u32,
    origin: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    let hook_was_invalidated = hook_handle.is_invalid();
    if !Seelen::is_running() {
        if !hook_was_invalidated {
            log::trace!("Exiting WinEventHook");
            let _ = unsafe { UnhookWinEvent(hook_handle) };
        }
        return;
    }

    if id_object != 0 {
        return;
    }

    if FULL_STATE.load().is_weg_enabled() {
        // raw events should be only used for a fastest and immediately processing
        log_error!(SeelenWeg::process_raw_win_event(event, origin));
    }

    let event = WinEvent::from(event);

    if event == WinEvent::ObjectLocationChange && !location_delay_completed(origin) {
        return;
    }

    // Follows lock order: CLI -> DATA -> EVENT to avoid deadlocks
    let mut seelen = trace_lock!(SEELEN);
    let mut hook_manager = trace_lock!(HOOK_MANAGER);
    hook_manager.event(event, origin, &mut seelen);

    if let Ok(synthetics) = event.get_synthetics(origin) {
        for synthetic_event in synthetics {
            hook_manager.event(synthetic_event, origin, &mut seelen)
        }
    }
}

pub fn register_win_hook() -> Result<()> {
    log::trace!("Registering Windows and Virtual Desktop Hooks");

    spawn_named_thread("WinEventHook", move || unsafe {
        SetWinEventHook(EVENT_MIN, EVENT_MAX, None, Some(win_event_hook), 0, 0, 0);
        let mut msg: MSG = MSG::default();
        loop {
            if !GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                break;
            };
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    })?;

    let (sender, receiver) = std::sync::mpsc::channel::<VirtualDesktopEvent>();
    get_vd_manager().listen_events(sender)?;
    spawn_named_thread("VirtualDesktopEventHook", move || {
        for event in receiver {
            log_error!(process_vd_event(event))
        }
    })?;

    spawn_named_thread("MouseEventHook", || {
        let handle = get_app_handle();
        let mut last_pos = Point::default();
        loop {
            if let Ok(pos) = Mouse::get_cursor_pos() {
                if last_pos != pos {
                    let _ = handle.emit(SeelenEvent::GlobalMouseMove, &[pos.get_x(), pos.get_y()]);
                    last_pos = pos;
                }
            }
            std::thread::sleep(Duration::from_millis(66)); // 15 FPS
        }
    })?;

    Ok(())
}
