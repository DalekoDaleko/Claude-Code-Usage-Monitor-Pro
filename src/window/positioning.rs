use super::*;
use std::sync::atomic::AtomicU32;

/// True while the surface has been lifted out of a full taskbar and is floating
/// above it. The renderer uses this to paint an opaque backdrop, since a
/// floating surface no longer has the taskbar behind its transparent pixels.
static FLOATING_FALLBACK: AtomicBool = AtomicBool::new(false);
/// Taskbar background colour sampled when the surface began floating, as
/// 0xAARRGGBB.
static FLOAT_BACKDROP: AtomicU32 = AtomicU32::new(0xFF20_2020);
/// True while the displayed readings are stale because polling has stopped.
static FROZEN_READINGS: AtomicBool = AtomicBool::new(false);

pub(super) fn position_at_taskbar() {
    refresh_dpi();
    let custom_position = {
        let state = lock_state();
        state.as_ref().and_then(|s| {
            if s.custom_theme_enabled {
                effective_theme_from_state(s).map(|mut theme| {
                    let runtime = theme_runtime_for_surface(&theme, 0, theme_runtime_from_state(s));
                    let (width, height) =
                        theme_engine::resolve_surface_size(&theme, 0, s.data.as_ref(), runtime);
                    theme.canvas.width = width;
                    theme.canvas.height = height;
                    let scale = theme_surface_scale(&theme, 0);
                    (s.hwnd.to_hwnd(), theme, scale)
                })
            } else {
                None
            }
        })
    };
    if let Some((hwnd, theme, scale)) = custom_position {
        position_custom_theme(hwnd, &theme, scale);
        return;
    }
    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (hwnd, embedded, tray_offset, taskbar_hwnd) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Don't fight the user's drag
        if s.dragging {
            return;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) => h.to_hwnd(),
            None => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return;
            }
        };

        (s.hwnd.to_hwnd(), s.embedded, s.tray_offset, taskbar_hwnd)
    };

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;
    let anchor_top = taskbar_rect.top;
    let anchor_height = taskbar_height;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }

    let widget_width = total_widget_width();
    let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
    let tray_offset = tray_offset.clamp(0, max_offset);
    let offset_changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.tray_offset != tray_offset {
                s.tray_offset = tray_offset;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if offset_changed {
        save_state_settings();
    }

    let widget_height = total_widget_height();
    let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        let x = tray_left - taskbar_rect.left - widget_width - tray_offset;
        native_interop::move_window(hwnd, x, y - taskbar_rect.top, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={} w={widget_width} h={widget_height}",
            y - taskbar_rect.top
        ));
    } else {
        // Topmost popup: screen coordinates
        let x = tray_left - widget_width - tray_offset;
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={x} y={y} w={widget_width} h={widget_height}"
        ));
    }
}

/// Whether the user has allowed the surface to be hosted inside the taskbar.
fn dock_in_taskbar_enabled() -> bool {
    lock_state()
        .as_ref()
        .is_some_and(|state| state.dock_in_taskbar)
}

/// Decide whether a taskbar-hosted surface has to float instead, and where.
///
/// Returns `Some((x, y))` in screen pixels when the surface is not to be hosted
/// inside the taskbar: either because docking is turned off, or because the
/// running-application strip leaves too little room for a `width`-wide surface
/// in front of the notification area. Returns `None` when docking is on and the
/// surface fits, or when docking is on and the taskbar cannot be measured — an
/// unmeasurable taskbar keeps the in-taskbar behaviour rather than relocating
/// the widget on a guess.
fn taskbar_float_placement(
    taskbar: &native_interop::TaskbarWindow,
    display: RECT,
    width: i32,
    height: i32,
) -> Option<(i32, i32)> {
    let docking = dock_in_taskbar_enabled();
    let layout = match crate::taskbar_layout::query(taskbar.hwnd) {
        Some(layout) if !docking || !layout.fits(width) => layout,
        // Fits, or the taskbar could not be measured: stay hosted in the
        // taskbar rather than relocating the widget on a guess.
        Some(_) => {
            set_floating_fallback(false, 0);
            return None;
        }
        None if docking => {
            set_floating_fallback(false, 0);
            return None;
        }
        // Docking is off and the taskbar could not be measured. Float anyway,
        // with no sampled backdrop: the measurement only decides the colour.
        None => {
            if !FLOATING_FALLBACK.load(Ordering::Relaxed) {
                diagnose::log("floating above the taskbar; docking is turned off");
            }
            set_floating_fallback(true, 0);
            return Some(clamp_float_origin(
                saved_float_origin()
                    .unwrap_or_else(|| default_float_origin(taskbar, display, width, height)),
                display,
                width,
                height,
            ));
        }
    };
    if !FLOATING_FALLBACK.load(Ordering::Relaxed) {
        if docking {
            diagnose::log(format!(
                "taskbar gap {}px is too small for a {width}px surface (apps end at {}, tray starts at {}, overflow button {}); floating above the taskbar",
                layout.available_width(),
                layout.apps_right,
                layout.tray_left,
                if layout.overflow_visible { "shown" } else { "hidden" }
            ));
        } else {
            diagnose::log("floating above the taskbar; docking is turned off");
        }
    }
    set_floating_fallback(true, sample_taskbar_backdrop(taskbar, &layout));
    Some(clamp_float_origin(
        saved_float_origin().unwrap_or_else(|| default_float_origin(taskbar, display, width, height)),
        display,
        width,
        height,
    ))
}

/// Where the widget floats before the user has dragged it: snapped to the right
/// edge of the screen, sitting directly above the taskbar.
///
/// The surface must clear the taskbar entirely. Both are topmost windows, so an
/// overlapping surface loses the z-order race whenever the shell is activated
/// and simply vanishes behind it.
fn default_float_origin(
    taskbar: &native_interop::TaskbarWindow,
    display: RECT,
    width: i32,
    height: i32,
) -> (i32, i32) {
    (display.right - width, taskbar.rect.top - height)
}

fn saved_float_origin() -> Option<(i32, i32)> {
    let state = lock_state();
    let state = state.as_ref()?;
    Some((state.float_x?, state.float_y?))
}

/// Keep a floating origin on screen so a display change cannot strand the
/// widget outside every monitor.
pub(super) fn clamp_float_origin(
    origin: (i32, i32),
    display: RECT,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let (x, y) = origin;
    let max_x = (display.right - width).max(display.left);
    let max_y = (display.bottom - height).max(display.top);
    (x.clamp(display.left, max_x), y.clamp(display.top, max_y))
}

pub(super) fn set_floating_fallback(active: bool, backdrop: u32) {
    FLOATING_FALLBACK.store(active, Ordering::Relaxed);
    if active {
        FLOAT_BACKDROP.store(backdrop, Ordering::Relaxed);
    }
}

pub(super) fn floating_fallback_active() -> bool {
    FLOATING_FALLBACK.load(Ordering::Relaxed)
}

/// True while the surface is showing readings that are no longer being
/// refreshed, because polling stopped before the displayed figures could be
/// replaced.
pub(super) fn frozen_readings() -> bool {
    FROZEN_READINGS.load(Ordering::Relaxed)
}

pub(super) fn set_frozen_readings(frozen: bool) {
    FROZEN_READINGS.store(frozen, Ordering::Relaxed);
}

/// Drop a premultiplied 0xAARRGGBB pixel to its luminance, keeping its alpha.
///
/// Rec. 601 luma weights, the same ones GDI and most desktop software use for
/// greyscale. Premultiplied channels stay premultiplied: scaling all three by
/// the same alpha commutes with the weighted sum.
pub(super) fn desaturate_if(pixel: u32, frozen: bool) -> u32 {
    if !frozen {
        return pixel;
    }
    let alpha = pixel & 0xFF00_0000;
    let r = (pixel >> 16) & 0xFF;
    let g = (pixel >> 8) & 0xFF;
    let b = pixel & 0xFF;
    // +500 rounds to nearest instead of truncating.
    let luma = ((299 * r + 587 * g + 114 * b) + 500) / 1000;
    let luma = luma.min(255);
    alpha | (luma << 16) | (luma << 8) | luma
}

/// Read the taskbar's own background colour so the floating surface can sit on
/// a matching opaque panel instead of showing whatever window is behind it.
///
/// The midpoint of the measured gap is guaranteed to be bare taskbar: it lies
/// between the last app button and the first tray icon.
fn sample_taskbar_backdrop(
    taskbar: &native_interop::TaskbarWindow,
    layout: &crate::taskbar_layout::TaskbarLayout,
) -> u32 {
    const FALLBACK: u32 = 0xFF20_2020;
    let x = (layout.apps_right + layout.tray_left) / 2;
    let y = taskbar.rect.top + (taskbar.rect.bottom - taskbar.rect.top) / 2;
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return FALLBACK;
        }
        let color = GetPixel(screen_dc, x, y);
        ReleaseDC(None, screen_dc);
        // GetPixel reports CLR_INVALID (0xFFFFFFFF) when the point is not on
        // the device surface.
        if color.0 == 0xFFFF_FFFF {
            return FALLBACK;
        }
        // COLORREF is 0x00BBGGRR; the render buffer is 0xAARRGGBB.
        let r = color.0 & 0xFF;
        let g = (color.0 >> 8) & 0xFF;
        let b = (color.0 >> 16) & 0xFF;
        0xFF00_0000 | (r << 16) | (g << 8) | b
    }
}

/// Composite one premultiplied 0xAARRGGBB source pixel over an opaque backdrop.
///
/// `UpdateLayeredWindow` with `ULW_ALPHA` consumes premultiplied colour, so the
/// source channels are already scaled by their own alpha and only the backdrop
/// needs weighting by the remaining coverage.
pub(super) fn composite_over(source: u32, backdrop: u32) -> u32 {
    let alpha = source >> 24;
    if alpha == 0xFF {
        return source;
    }
    let inverse = 255 - alpha;
    let blend = |shift: u32| {
        let src = (source >> shift) & 0xFF;
        let dst = (backdrop >> shift) & 0xFF;
        // +127 rounds to nearest rather than truncating, which keeps flat fills
        // from drifting a shade darker than the taskbar they sit against.
        (src + (dst * inverse + 127) / 255).min(255) << shift
    };
    0xFF00_0000 | blend(16) | blend(8) | blend(0)
}

#[cfg(test)]
mod float_tests {
    use super::*;

    const SCREEN: RECT = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    #[test]
    fn opaque_backdrop_shows_through_transparent_pixels() {
        let backdrop = 0xFF1E_3A3A;
        // A fully transparent source must come out as the backdrop itself, so
        // the floating panel reads as solid rather than showing the desktop.
        assert_eq!(composite_over(0x0000_0000, backdrop), backdrop);
    }

    #[test]
    fn opaque_source_pixels_are_untouched() {
        assert_eq!(composite_over(0xFFAB_CDEF, 0xFF1E_3A3A), 0xFFAB_CDEF);
    }

    #[test]
    fn composite_always_yields_full_alpha() {
        for alpha in [0x00u32, 0x01, 0x7F, 0xFE, 0xFF] {
            let source = (alpha << 24) | 0x0010_2030;
            assert_eq!(composite_over(source, 0xFF40_5060) >> 24, 0xFF);
        }
    }

    #[test]
    fn composite_channels_never_overflow() {
        // Premultiplied white at half coverage over white must clamp, not wrap.
        let result = composite_over(0x80FF_FFFF, 0xFFFF_FFFF);
        assert_eq!(result, 0xFFFF_FFFF);
    }

    #[test]
    fn frozen_readings_render_without_colour() {
        // Pure red, green and blue must collapse onto the grey axis.
        for colour in [0xFFFF_0000u32, 0xFF00_FF00, 0xFF00_00FF, 0xFF12_3456] {
            let grey = desaturate_if(colour, true);
            let r = (grey >> 16) & 0xFF;
            let g = (grey >> 8) & 0xFF;
            let b = grey & 0xFF;
            assert_eq!(r, g, "channels must match for {colour:#010x}");
            assert_eq!(g, b, "channels must match for {colour:#010x}");
        }
    }

    #[test]
    fn desaturation_preserves_alpha_and_is_opt_in() {
        // Alpha carries the surface's shape and hit-testing, so it must survive.
        assert_eq!(desaturate_if(0x80FF_0000, true) >> 24, 0x80);
        assert_eq!(desaturate_if(0x0012_3456, true) >> 24, 0x00);
        // Live readings keep every colour untouched.
        assert_eq!(desaturate_if(0xFF12_3456, false), 0xFF12_3456);
    }

    #[test]
    fn desaturation_matches_rec601_luma() {
        // The widget's orange accent, and pure white which must stay white.
        assert_eq!(desaturate_if(0xFFFF_FFFF, true), 0xFFFF_FFFF);
        assert_eq!(desaturate_if(0xFF00_0000, true), 0xFF00_0000);
        // 0.299*255 = 76.2 -> 76
        assert_eq!(desaturate_if(0xFFFF_0000, true) & 0xFF, 76);
        // 0.587*255 = 149.7 -> 150
        assert_eq!(desaturate_if(0xFF00_FF00, true) & 0xFF, 150);
    }

    #[test]
    fn default_origin_snaps_to_the_right_screen_edge() {
        let taskbar = native_interop::TaskbarWindow {
            hwnd: HWND::default(),
            rect: RECT {
                left: 0,
                top: 1032,
                right: 1920,
                bottom: 1080,
            },
        };
        // Flush with the right edge, clearing the 48px-tall taskbar.
        assert_eq!(
            default_float_origin(&taskbar, SCREEN, 217, 46),
            (1703, 986)
        );
    }

    #[test]
    fn dragged_origin_stays_on_screen() {
        assert_eq!(
            clamp_float_origin((5000, 5000), SCREEN, 217, 46),
            (1703, 1034)
        );
        assert_eq!(clamp_float_origin((-400, -400), SCREEN, 217, 46), (0, 0));
        assert_eq!(
            clamp_float_origin((800, 500), SCREEN, 217, 46),
            (800, 500),
            "a position already on screen must not be moved"
        );
    }

    #[test]
    fn clamp_survives_a_surface_wider_than_the_screen() {
        // max_x would go negative without the guard, and clamp() panics when
        // its low bound exceeds its high bound.
        assert_eq!(clamp_float_origin((100, 100), SCREEN, 4000, 46), (0, 100));
    }
}

pub(super) fn reset_layered_window(hwnd: HWND) {
    unsafe {
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style & !(WS_EX_LAYERED.0 as i32));
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_LAYERED.0 as i32);
    }
}

pub(super) fn render_desktop_custom_window(hwnd: HWND, rendered: &theme_engine::RenderedTheme) {
    if let Err(error) = crate::desktop_compositor::present(hwnd, rendered) {
        diagnose::log(format!(
            "desktop theme render failed hwnd={:?} size={}x{} error={error}",
            hwnd, rendered.width, rendered.height
        ));
    }
}

pub(super) fn render_custom_window(
    hwnd: HWND,
    rendered: &theme_engine::RenderedTheme,
    desktop_nested: bool,
) {
    if desktop_nested {
        render_desktop_custom_window(hwnd, rendered);
        return;
    }

    let width = rendered.width as i32;
    let height = rendered.height as i32;
    unsafe {
        // SetLayeredWindowAttributes and UpdateLayeredWindow cannot be used on
        // the same layered-style lifetime. Reset it in case this surface was
        // previously hosted on the desktop.
        reset_layered_window(hwnd);
        // UpdateLayeredWindow expects a screen-compatible destination DC. A
        // window DC happened to work for taskbar-hosted children, but desktop
        // WorkerW/DefView composition can discard the resulting surface.
        let screen_dc = GetDC(None);
        let memory_dc = CreateCompatibleDC(Some(screen_dc));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(memory_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .unwrap_or_default();
        if bitmap.is_invalid() || bits.is_null() {
            let _ = DeleteDC(memory_dc);
            ReleaseDC(None, screen_dc);
            return;
        }
        let old = SelectObject(memory_dc, bitmap.into());
        let window_pixels = std::slice::from_raw_parts_mut(bits as *mut u32, rendered.pixels.len());
        // A taskbar-hosted surface shows the taskbar through its transparent
        // pixels. Once it floats clear of the taskbar there is nothing behind
        // it but whatever window happens to be there, so composite the theme
        // over an opaque panel in the taskbar's own colour instead.
        // Figures that are no longer being refreshed are drawn without colour,
        // so a surface that has quietly stopped updating cannot be mistaken for
        // a live one.
        let frozen = frozen_readings();
        if floating_fallback_active() {
            let backdrop = FLOAT_BACKDROP.load(Ordering::Relaxed);
            for (target, source) in window_pixels.iter_mut().zip(&rendered.pixels) {
                *target = composite_over(desaturate_if(*source, frozen), backdrop);
            }
        } else {
            for (target, source) in window_pixels.iter_mut().zip(&rendered.pixels) {
                // Windows normally lets mouse input pass through zero-alpha pixels in
                // layered windows. A nearly transparent pixel keeps the full surface
                // interactive without changing the theme renderer's pixel output.
                *target = if source >> 24 == 0 {
                    0x0100_0000
                } else {
                    desaturate_if(*source, frozen)
                };
            }
        }
        let source = POINT { x: 0, y: 0 };
        let size = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        if let Err(error) = UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            None,
            Some(&size),
            Some(memory_dc),
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            diagnose::log(format!(
                "custom theme render failed hwnd={:?} size={}x{} error={error}",
                hwnd, width, height
            ));
        }
        SelectObject(memory_dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory_dc);
        ReleaseDC(None, screen_dc);
    }
}

pub(super) fn position_custom_theme(hwnd: HWND, theme: &ThemeDocument, scale: f64) {
    position_custom_theme_internal(hwnd, theme, scale);
}

pub(super) fn position_custom_theme_internal(hwnd: HWND, theme: &ThemeDocument, scale: f64) {
    let taskbars = native_interop::find_taskbars();
    let displays = native_interop::find_monitors();
    let display_index = theme.placement.reference.display;
    let selected_display = displays
        .get(display_index)
        .copied()
        .or_else(|| displays.first().copied());
    let Some(display) = selected_display else {
        return;
    };
    let taskbar = taskbars.iter().find(|taskbar| unsafe {
        MonitorFromWindow(taskbar.hwnd, MONITOR_DEFAULTTOPRIMARY) == display.handle
    });
    let reference = match theme.placement.reference.region {
        ReferenceRegion::Monitor => display.rect,
        ReferenceRegion::Taskbar => taskbar.map(|taskbar| taskbar.rect).unwrap_or(display.rect),
        ReferenceRegion::SystemTray => taskbar
            .and_then(|taskbar| {
                native_interop::find_child_window(taskbar.hwnd, "TrayNotifyWnd")
                    .and_then(native_interop::get_window_rect_safe)
                    .or(Some(taskbar.rect))
            })
            .unwrap_or(display.rect),
    };
    let width = scaled_theme_dimension(theme.canvas.width.max(1), scale);
    let height = scaled_theme_dimension(theme.canvas.height.max(1), scale);
    let reference_width = reference.right - reference.left;
    let reference_height = reference.bottom - reference.top;
    let surface_horizontal = theme
        .placement
        .surface_horizontal
        .unwrap_or(theme.placement.horizontal);
    let surface_vertical = theme
        .placement
        .surface_vertical
        .unwrap_or(theme.placement.vertical);
    let x = aligned_origin(
        reference.left,
        reference_width,
        width,
        horizontal_anchor_factor(theme.placement.horizontal),
        horizontal_anchor_factor(surface_horizontal),
        (theme.placement.offset_x as f64 * scale).round() as i32,
    );
    let y = aligned_origin(
        reference.top,
        reference_height,
        height,
        vertical_anchor_factor(theme.placement.vertical),
        vertical_anchor_factor(surface_vertical),
        (theme.placement.offset_y as f64 * scale).round() as i32,
    );
    let nest = theme
        .placement
        .nest
        .resolve(theme.placement.reference.region);
    unsafe {
        match nest {
            SurfaceNest::Taskbar => {
                let Some(taskbar) = taskbar else {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    return;
                };
                // Hosting inside the taskbar is only correct while the strip of
                // running-application buttons leaves a wide enough gap in front
                // of the notification area. When it does not, sitting there
                // would cover the app buttons and the "..." overflow button,
                // so float just above the taskbar instead.
                if let Some(float) = taskbar_float_placement(taskbar, display.rect, width, height) {
                    native_interop::make_popup(hwnd, true);
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        float.0,
                        float.1,
                        width,
                        height,
                        SWP_NOACTIVATE,
                    );
                    return;
                }
                native_interop::embed_as_child(hwnd, taskbar.hwnd);
                let mut point = [POINT { x, y }];
                MapWindowPoints(None, Some(taskbar.hwnd), &mut point);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOP),
                    point[0].x,
                    point[0].y,
                    width,
                    height,
                    SWP_NOACTIVATE,
                );
            }
            SurfaceNest::Desktop => {
                // Only a taskbar-hosted surface is ever lifted out to float, so
                // every other nest must clear the flag or a stale value would
                // keep forcing this window topmost and painting a backdrop.
                set_floating_fallback(false, 0);
                if let Some(desktop) = native_interop::find_desktop_host() {
                    if GetParent(hwnd).ok() != Some(desktop.parent) {
                        native_interop::embed_as_child(hwnd, desktop.parent);
                    }
                    let mut point = [POINT { x, y }];
                    MapWindowPoints(None, Some(desktop.parent), &mut point);
                    let _ = SetWindowPos(
                        hwnd,
                        Some(desktop.insert_after),
                        point[0].x,
                        point[0].y,
                        width,
                        height,
                        SWP_NOACTIVATE,
                    );
                } else {
                    native_interop::make_popup(hwnd, false);
                    let _ =
                        SetWindowPos(hwnd, Some(HWND_BOTTOM), x, y, width, height, SWP_NOACTIVATE);
                }
            }
            SurfaceNest::TrayIcon => {
                set_floating_fallback(false, 0);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            SurfaceNest::Floating | SurfaceNest::Auto => {
                // A deliberately floating theme draws its own background, so it
                // keeps the theme's own transparency rather than the taskbar
                // backdrop used by the full-taskbar fallback.
                set_floating_fallback(false, 0);
                native_interop::make_popup(hwnd, true);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE,
                );
            }
        }
    }
}

pub(super) fn sync_theme_window_visibility() {
    let (theme, data, runtime, windows) = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return;
        };
        if !state.custom_theme_enabled {
            return;
        }
        let Some(theme) = effective_theme_from_state(state) else {
            return;
        };
        (
            theme,
            state.data.clone(),
            theme_runtime_from_state(state),
            std::iter::once(state.hwnd)
                .chain(state.mirror_hwnds.iter().copied())
                .collect::<Vec<_>>(),
        )
    };
    unsafe {
        for (surface_index, surface) in theme.surfaces.iter().enumerate() {
            let nest = surface
                .placement
                .nest
                .resolve(surface.placement.reference.region);
            if nest != SurfaceNest::Floating {
                continue;
            }
            let Some(regular_window) = windows.get(surface_index) else {
                continue;
            };
            let hwnd = regular_window.to_hwnd();
            if !IsWindow(Some(hwnd)).as_bool() {
                continue;
            }
            let surface_runtime = theme_runtime_for_surface(&theme, surface_index, runtime);
            let should_show =
                theme_engine::surface_should_render(
                    &theme,
                    surface_index,
                    data.as_ref(),
                    surface_runtime,
                ) && !foreground_is_fullscreen_on_display(surface.placement.reference.display);
            if should_show == IsWindowVisible(hwnd).as_bool() {
                continue;
            }
            if should_show {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            let _ = ShowWindow(
                hwnd,
                if should_show {
                    SW_SHOWNOACTIVATE
                } else {
                    SW_HIDE
                },
            );
        }
    }
}

pub(super) fn foreground_is_fullscreen_on_display(display_index: usize) -> bool {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid()
            || !IsWindowVisible(foreground).as_bool()
            || IsIconic(foreground).as_bool()
        {
            return false;
        }
        let class = native_interop::window_class_name(foreground).unwrap_or_default();
        if matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            return false;
        }
        let is_ours = {
            let state = lock_state();
            state.as_ref().is_some_and(|state| {
                state.hwnd.to_hwnd() == foreground
                    || state
                        .mirror_hwnds
                        .iter()
                        .any(|window| window.to_hwnd() == foreground)
                    || state
                        .desktop_hwnds
                        .iter()
                        .flatten()
                        .any(|window| window.to_hwnd() == foreground)
            })
        };
        if is_ours {
            return false;
        }

        let displays = native_interop::find_monitors();
        let Some(display) = displays
            .get(display_index)
            .copied()
            .or_else(|| displays.first().copied())
        else {
            return false;
        };
        if MonitorFromWindow(foreground, MONITOR_DEFAULTTONULL) != display.handle {
            return false;
        }
        let mut rect = RECT::default();
        if DwmGetWindowAttribute(
            foreground,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(foreground, &mut rect).is_err()
        {
            return false;
        }
        rect_covers_monitor(rect, display.rect)
    }
}

pub(super) fn rect_covers_monitor(rect: RECT, monitor: RECT) -> bool {
    const EDGE_TOLERANCE: i32 = 2;
    rect.left <= monitor.left + EDGE_TOLERANCE
        && rect.top <= monitor.top + EDGE_TOLERANCE
        && rect.right >= monitor.right - EDGE_TOLERANCE
        && rect.bottom >= monitor.bottom - EDGE_TOLERANCE
}

pub(super) fn aligned_origin(
    reference_start: i32,
    reference_length: i32,
    surface_length: i32,
    reference_factor: f64,
    surface_factor: f64,
    offset: i32,
) -> i32 {
    (reference_start as f64 + reference_length as f64 * reference_factor
        - surface_length as f64 * surface_factor)
        .round() as i32
        + offset
}

pub(super) fn horizontal_anchor_factor(anchor: HorizontalAnchor) -> f64 {
    match anchor {
        HorizontalAnchor::Left => 0.0,
        HorizontalAnchor::Center => 0.5,
        HorizontalAnchor::Right => 1.0,
    }
}

pub(super) fn vertical_anchor_factor(anchor: VerticalAnchor) -> f64 {
    match anchor {
        VerticalAnchor::Top => 0.0,
        VerticalAnchor::Center => 0.5,
        VerticalAnchor::Bottom => 1.0,
    }
}

pub(super) fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

/// WinEvent callback for tray icon location changes
pub(super) unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h.to_hwnd() == hwnd)
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            refresh_theme_host_geometry();
            position_at_taskbar();
            render_layered();
        }
    }
}
