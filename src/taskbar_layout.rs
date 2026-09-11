//! Measures how much free room the Windows 11 taskbar actually has between the
//! running-application buttons and the notification area.
//!
//! The legacy `ReBarWindow32` / `MSTaskListWClass` windows still exist on
//! Windows 11, but they are compatibility shims: `ReBarWindow32` reports a
//! fixed container span regardless of how many buttons are shown, and
//! `MSTaskListWClass` is not even visible. Reading them makes a full taskbar
//! look identical to an empty one, so the widget happily parks itself on top of
//! the running-app buttons and the "..." overflow button.
//!
//! UI Automation reports the real XAML layout. Every taskbar element carries a
//! stable `AutomationId`:
//!
//! - `StartButton`, `Appid: ...`, `Window: ...` — the running-app strip
//! - `OverflowButton` — the "..." button, present only when the strip is full
//! - `SystemTrayIcon` — clock, input indicator, hidden-icon chevron
//!
//! The gap between the right edge of the last app-strip element and the left
//! edge of the leftmost tray icon is the space the widget may occupy.

use crate::diagnose;
use std::cell::Cell;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Descendants,
};

/// How long a measurement stays usable before the taskbar is queried again.
///
/// Repositioning is driven by `EVENT_OBJECT_LOCATIONCHANGE`, which fires for
/// every window move on the desktop, so an un-throttled query would run UI
/// Automation cross-process calls dozens of times a second on the UI thread.
const CACHE_TTL: Duration = Duration::from_millis(500);

/// Horizontal breathing room kept between the app strip and the widget.
const EDGE_PADDING: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskbarLayout {
    /// Right edge of the rightmost running-app element, in screen pixels.
    /// Includes the "..." overflow button when Windows is showing one.
    pub apps_right: i32,
    /// Left edge of the leftmost notification-area icon, in screen pixels.
    pub tray_left: i32,
    /// Whether Windows is showing the "..." taskbar overflow button, which
    /// means the app strip has already run out of room.
    pub overflow_visible: bool,
}

impl TaskbarLayout {
    /// Width available to the widget between the app strip and the tray.
    pub fn available_width(&self) -> i32 {
        (self.tray_left - self.apps_right - EDGE_PADDING).max(0)
    }

    /// Whether a widget of `width` can sit in the gap without covering the app
    /// buttons or the overflow button.
    pub fn fits(&self, width: i32) -> bool {
        width <= self.available_width()
    }
}

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
    static CACHE: Cell<Option<(isize, Instant, Option<TaskbarLayout>)>> = const { Cell::new(None) };
}

/// Measure the taskbar, reusing a recent result when one is available.
///
/// Returns `None` when UI Automation cannot describe the taskbar, in which case
/// callers should keep their previous behaviour rather than guess.
pub fn query(taskbar_hwnd: HWND) -> Option<TaskbarLayout> {
    let key = taskbar_hwnd.0 as isize;
    let cached = CACHE.with(|cache| cache.get());
    if let Some((cached_key, measured_at, layout)) = cached {
        if cached_key == key && measured_at.elapsed() < CACHE_TTL {
            return layout;
        }
    }

    let layout = measure(taskbar_hwnd);
    CACHE.with(|cache| cache.set(Some((key, Instant::now(), layout))));
    layout
}

/// Discard the cached measurement so the next `query` re-reads the taskbar.
///
/// Used when the shell tells us the layout changed and we want the new geometry
/// immediately instead of up to `CACHE_TTL` later.
pub fn invalidate() {
    CACHE.with(|cache| cache.set(None));
}

fn ensure_com() -> bool {
    COM_READY.with(|ready| {
        if ready.get() {
            return true;
        }
        // Returns S_FALSE when this thread is already initialised, which is
        // still success for our purposes. A genuine failure means another
        // apartment model is in force and UI Automation is unavailable here.
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_err() {
            diagnose::log(format!(
                "taskbar layout: CoInitializeEx failed ({result:?}); falling back to legacy placement"
            ));
            return false;
        }
        ready.set(true);
        true
    })
}

fn measure(taskbar_hwnd: HWND) -> Option<TaskbarLayout> {
    if !ensure_com() {
        return None;
    }

    unsafe {
        let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| diagnose::log(format!("taskbar layout: no UI Automation ({error})")))
            .ok()?;

        let root = automation
            .ElementFromHandle(taskbar_hwnd)
            .map_err(|error| {
                diagnose::log(format!("taskbar layout: ElementFromHandle failed ({error})"))
            })
            .ok()?;

        let condition = automation.CreateTrueCondition().ok()?;
        let elements = root
            .FindAll(TreeScope_Descendants, &condition)
            .map_err(|error| diagnose::log(format!("taskbar layout: FindAll failed ({error})")))
            .ok()?;

        let count = elements.Length().unwrap_or(0);
        let mut apps_right: Option<i32> = None;
        let mut tray_left: Option<i32> = None;
        let mut overflow_visible = false;

        for index in 0..count {
            let Ok(element) = elements.GetElement(index) else {
                continue;
            };
            let Some(automation_id) = automation_id(&element) else {
                continue;
            };
            let Some((left, right)) = horizontal_bounds(&element) else {
                continue;
            };
            // Collapsed elements report a zero-width rectangle; they occupy no
            // space and must not drag the boundaries around.
            if right <= left {
                continue;
            }

            match classify(&automation_id) {
                Some(Slot::AppStrip { overflow }) => {
                    overflow_visible |= overflow;
                    apps_right = Some(apps_right.map_or(right, |current: i32| current.max(right)));
                }
                Some(Slot::Tray) => {
                    tray_left = Some(tray_left.map_or(left, |current: i32| current.min(left)));
                }
                None => {}
            }
        }

        let layout = TaskbarLayout {
            apps_right: apps_right?,
            tray_left: tray_left?,
            overflow_visible,
        };
        if layout.tray_left <= layout.apps_right {
            // A vertical or stacked taskbar puts the tray below rather than
            // beside the app strip, so a horizontal gap is meaningless.
            return None;
        }
        Some(layout)
    }
}

enum Slot {
    AppStrip { overflow: bool },
    Tray,
}

fn classify(automation_id: &str) -> Option<Slot> {
    if automation_id == "SystemTrayIcon" {
        return Some(Slot::Tray);
    }
    if automation_id == "OverflowButton" {
        return Some(Slot::AppStrip { overflow: true });
    }
    if automation_id == "StartButton"
        || automation_id.starts_with("Window: ")
        || automation_id.starts_with("Appid: ")
    {
        return Some(Slot::AppStrip { overflow: false });
    }
    None
}

fn automation_id(element: &IUIAutomationElement) -> Option<String> {
    unsafe {
        element
            .CurrentAutomationId()
            .ok()
            .map(|text| text.to_string())
            .filter(|text| !text.is_empty())
    }
}

/// Read an element's bounding rectangle as `(left, right)` screen pixels.
fn horizontal_bounds(element: &IUIAutomationElement) -> Option<(i32, i32)> {
    unsafe {
        let bounds = element.CurrentBoundingRectangle().ok()?;
        Some((bounds.left, bounds.right))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(apps_right: i32, tray_left: i32) -> TaskbarLayout {
        TaskbarLayout {
            apps_right,
            tray_left,
            overflow_visible: false,
        }
    }

    #[test]
    fn available_width_subtracts_padding() {
        assert_eq!(layout(1000, 1200).available_width(), 200 - EDGE_PADDING);
    }

    #[test]
    fn available_width_never_goes_negative() {
        assert_eq!(layout(1200, 1000).available_width(), 0);
        assert_eq!(layout(1200, 1201).available_width(), 0);
    }

    #[test]
    fn full_taskbar_rejects_the_widget() {
        // Measured on a full 1920px Windows 11 taskbar: the "..." overflow
        // button ends at 1650 and the tray starts at 1697.
        let full = layout(1650, 1697);
        assert!(!full.fits(217), "217px widget must not fit in a 47px gap");
        assert!(full.fits(40));
    }

    #[test]
    fn empty_taskbar_accepts_the_widget() {
        let roomy = layout(600, 1697);
        assert!(roomy.fits(217));
    }

    #[test]
    fn classify_recognises_taskbar_elements() {
        assert!(matches!(classify("SystemTrayIcon"), Some(Slot::Tray)));
        assert!(matches!(
            classify("OverflowButton"),
            Some(Slot::AppStrip { overflow: true })
        ));
        assert!(matches!(
            classify("Window: 0x106b6"),
            Some(Slot::AppStrip { overflow: false })
        ));
        assert!(matches!(
            classify("Appid: Claude_pzs8sxrjxfjjc!Claude"),
            Some(Slot::AppStrip { overflow: false })
        ));
        assert!(matches!(
            classify("StartButton"),
            Some(Slot::AppStrip { overflow: false })
        ));
        assert!(classify("TaskbarFrame").is_none());
        assert!(classify("").is_none());
    }
}
