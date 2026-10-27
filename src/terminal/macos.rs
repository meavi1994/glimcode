//! Read the cursor from a valid Quartz event. No event is posted or injected.
use eframe::egui;
use std::{ffi::c_void, ptr::NonNull};

#[repr(C)]
struct Point {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *const c_void) -> Point;
    #[cfg(test)]
    fn CGEventSetLocation(event: *mut c_void, location: Point);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
}

struct CursorEvent(NonNull<c_void>);

impl CursorEvent {
    fn new() -> Option<Self> {
        // A null *source* selects the default event source. In contrast, a
        // null *event* passed to CGEventGetLocation always returns CGPointZero.
        NonNull::new(unsafe { CGEventCreate(std::ptr::null()) }).map(Self)
    }

    fn location(&self) -> egui::Pos2 {
        // The event was checked at construction and is retained until Drop.
        let point = unsafe { CGEventGetLocation(self.0.as_ptr()) };
        egui::pos2(point.x as f32, point.y as f32)
    }
}

impl Drop for CursorEvent {
    fn drop(&mut self) {
        // CGEventCreate transfers ownership of one Core Foundation reference.
        unsafe { CFRelease(self.0.as_ptr()) };
    }
}

pub(super) fn cursor_position() -> Option<egui::Pos2> {
    Some(CursorEvent::new()?.location())
}

pub(super) fn window_position(cursor: egui::Pos2, window: egui::Rect, zoom: f32) -> egui::Pos2 {
    cursor / zoom - window.min.to_vec2()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_drop_position_reads_a_valid_event_instead_of_a_null_event() {
        let event = CursorEvent::new().expect("create a local Quartz event");
        // Only change this local event object; do not move the system cursor.
        unsafe {
            CGEventSetLocation(
                event.0.as_ptr(),
                Point {
                    x: 732.0,
                    y: 1050.0,
                },
            )
        };
        assert_eq!(event.location(), egui::pos2(732.0, 1050.0));
        let invalid = unsafe { CGEventGetLocation(std::ptr::null()) };
        assert_eq!(
            (invalid.x, invalid.y),
            (0.0, 0.0),
            "the previous null-event call cannot locate a drop"
        );
        let window = egui::Rect::from_min_size(egui::pos2(200.0, 500.0), egui::vec2(800.0, 600.0));
        assert_eq!(
            window_position(event.location(), window, 1.0),
            egui::pos2(532.0, 550.0)
        );
    }

    #[test]
    fn drop_coordinates_account_for_zoom_and_windows_on_other_displays() {
        let window = egui::Rect::from_min_size(egui::pos2(-400.0, 100.0), egui::vec2(800.0, 600.0));
        assert_eq!(
            window_position(egui::pos2(-600.0, 400.0), window, 2.0),
            egui::pos2(100.0, 100.0)
        );
    }
}
