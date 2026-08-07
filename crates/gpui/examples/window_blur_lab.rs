#![cfg_attr(target_family = "wasm", no_main)]
//! A/B lab for macOS window-background blur mechanisms.
//!
//! Opens one labeled, transparent window per candidate blur mechanism and applies that
//! mechanism to the window's `NSWindow`. A human runs this over a patterned desktop
//! wallpaper and reports which numbered windows actually frost what is behind them,
//! which is the only way to tell which mechanism still works on the linked macOS SDK.

use gpui::{
    App, Bounds, Context, SharedString, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, div, point, prelude::*, px, rgba, size, white,
};
use gpui_platform::application;

/// Raw `NSVisualEffectMaterial` values. Used raw rather than via the `cocoa` crate's enum so
/// that adding a material here does not depend on that enum covering it.
const MATERIAL_MENU: isize = 5;
const MATERIAL_SIDEBAR: isize = 7;
const MATERIAL_SELECTION: isize = 4;
const MATERIAL_HUD_WINDOW: isize = 13;
const MATERIAL_FULL_SCREEN_UI: isize = 15;
const MATERIAL_UNDER_WINDOW_BACKGROUND: isize = 21;

#[derive(Clone, Copy)]
enum Mechanism {
    /// What gpui does today: `WindowBackgroundAppearance::Blurred`, which installs an
    /// `NSVisualEffectView` subclass using the `Selection` material and additionally hides the
    /// chameleon layer and strips the saturation filter.
    GpuiBlurred,
    /// Control: a transparent window with nothing added, to show what "no frost" looks like.
    TransparentOnly,
    /// A stock `NSVisualEffectView` (no layer surgery) behind gpui's content view.
    VisualEffect { material: isize },
    /// The private CoreGraphics/SkyLight window-server blur, resolved at runtime.
    PrivateWindowServerBlur { radius: i32 },
    /// macOS 26+ Liquid Glass view, if this AppKit has it.
    GlassEffect,
}

const CANDIDATES: &[(&str, Mechanism)] = &[
    ("1 gpui Blurred (baseline)", Mechanism::GpuiBlurred),
    ("2 transparent only (control)", Mechanism::TransparentOnly),
    (
        "3 VisualEffect UnderWindowBackground",
        Mechanism::VisualEffect {
            material: MATERIAL_UNDER_WINDOW_BACKGROUND,
        },
    ),
    (
        "4 VisualEffect HUDWindow",
        Mechanism::VisualEffect {
            material: MATERIAL_HUD_WINDOW,
        },
    ),
    (
        "5 VisualEffect Sidebar",
        Mechanism::VisualEffect {
            material: MATERIAL_SIDEBAR,
        },
    ),
    (
        "6 VisualEffect FullScreenUI",
        Mechanism::VisualEffect {
            material: MATERIAL_FULL_SCREEN_UI,
        },
    ),
    (
        "7 VisualEffect Menu",
        Mechanism::VisualEffect {
            material: MATERIAL_MENU,
        },
    ),
    (
        "8 VisualEffect Selection, no layer surgery",
        Mechanism::VisualEffect {
            material: MATERIAL_SELECTION,
        },
    ),
    (
        "9 private window-server blur",
        Mechanism::PrivateWindowServerBlur { radius: 30 },
    ),
    ("10 NSGlassEffectView", Mechanism::GlassEffect),
];

const WINDOW_WIDTH: f32 = 320.;
const WINDOW_HEIGHT: f32 = 220.;
const WINDOW_GAP: f32 = 16.;
const GRID_COLUMNS: usize = 4;
const GRID_ORIGIN_X: f32 = 32.;
const GRID_ORIGIN_Y: f32 = 64.;

struct Candidate {
    label: &'static str,
    mechanism: Mechanism,
    applied: bool,
    status: SharedString,
}

impl Render for Candidate {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.applied {
            // Applied on first render rather than at window construction, so that the
            // platform window's content view is guaranteed to exist.
            self.applied = true;
            self.status = apply(self.mechanism, window);
            cx.notify();
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_1()
            .border_color(rgba(0xffffff99))
            .text_color(white())
            .child(
                div()
                    .bg(rgba(0x000000cc))
                    .rounded_md()
                    .px_2()
                    .py_1()
                    .text_size(px(15.))
                    .child(self.label),
            )
            .child(
                div()
                    .bg(rgba(0x000000aa))
                    .rounded_md()
                    .px_2()
                    .py_1()
                    .text_size(px(11.))
                    .child(self.status.clone()),
            )
    }
}

#[cfg(target_os = "macos")]
fn apply(mechanism: Mechanism, window: &Window) -> SharedString {
    macos::apply(mechanism, window)
}

#[cfg(not(target_os = "macos"))]
fn apply(_mechanism: Mechanism, _window: &Window) -> SharedString {
    "this lab only does anything on macOS".into()
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Mechanism;
    use cocoa::{
        appkit::{NSView, NSViewHeightSizable, NSViewWidthSizable, NSWindow, NSWindowOrderingMode},
        base::{id, nil},
        foundation::NSInteger,
    };
    use gpui::{SharedString, Window};
    use objc::{class, msg_send, runtime::Class, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{
        ffi::{CStr, c_char, c_int, c_void},
        mem,
    };

    const BLENDING_MODE_BEHIND_WINDOW: isize = 0;
    const STATE_ACTIVE: isize = 1;

    pub fn apply(mechanism: Mechanism, window: &Window) -> SharedString {
        let Some(native_window) = native_window(window) else {
            return "could not reach the NSWindow".into();
        };

        match mechanism {
            Mechanism::GpuiBlurred => "gpui WindowBackgroundAppearance::Blurred".into(),
            Mechanism::TransparentOnly => "transparent window, no blur requested".into(),
            Mechanism::VisualEffect { material } => add_visual_effect_view(native_window, material),
            Mechanism::PrivateWindowServerBlur { radius } => {
                set_window_server_blur(native_window, radius)
            }
            Mechanism::GlassEffect => add_glass_effect_view(native_window),
        }
    }

    fn native_window(window: &Window) -> Option<id> {
        // `Window` has an inherent `window_handle` returning gpui's own handle type, so the
        // raw-window-handle trait method has to be named explicitly.
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(appkit_handle) = handle.as_raw() else {
            return None;
        };
        let native_view = appkit_handle.ns_view.as_ptr() as id;
        if native_view.is_null() {
            return None;
        }
        let native_window: id = unsafe { msg_send![native_view, window] };
        (!native_window.is_null()).then_some(native_window)
    }

    fn add_visual_effect_view(native_window: id, material: isize) -> SharedString {
        unsafe {
            let content_view = native_window.contentView();
            if content_view.is_null() {
                return "window has no content view".into();
            }
            let frame = NSView::bounds(content_view);
            let view: id = msg_send![class!(NSVisualEffectView), alloc];
            let view: id = msg_send![view, initWithFrame: frame];
            if view.is_null() {
                return "NSVisualEffectView could not be created".into();
            }
            let view: id = msg_send![view, autorelease];
            let _: () = msg_send![view, setMaterial: material];
            let _: () = msg_send![view, setBlendingMode: BLENDING_MODE_BEHIND_WINDOW];
            let _: () = msg_send![view, setState: STATE_ACTIVE];
            view.setAutoresizingMask_(NSViewWidthSizable | NSViewHeightSizable);
            let _: () = msg_send![
                content_view,
                addSubview: view
                positioned: NSWindowOrderingMode::NSWindowBelow
                relativeTo: nil
            ];
            format!("NSVisualEffectView material={material}, BehindWindow, Active").into()
        }
    }

    fn add_glass_effect_view(native_window: id) -> SharedString {
        let Some(glass_class) = Class::get("NSGlassEffectView") else {
            return "NSGlassEffectView is not present in this AppKit".into();
        };
        unsafe {
            let content_view = native_window.contentView();
            if content_view.is_null() {
                return "window has no content view".into();
            }
            let frame = NSView::bounds(content_view);
            let view: id = msg_send![glass_class, alloc];
            let view: id = msg_send![view, initWithFrame: frame];
            if view.is_null() {
                return "NSGlassEffectView could not be created".into();
            }
            let view: id = msg_send![view, autorelease];
            view.setAutoresizingMask_(NSViewWidthSizable | NSViewHeightSizable);
            let _: () = msg_send![
                content_view,
                addSubview: view
                positioned: NSWindowOrderingMode::NSWindowBelow
                relativeTo: nil
            ];
            "NSGlassEffectView added behind the content".into()
        }
    }

    type ConnectionId = c_int;
    type DefaultConnectionForThread = unsafe extern "C" fn() -> ConnectionId;
    type SetWindowBackgroundBlurRadius = unsafe extern "C" fn(ConnectionId, u32, c_int) -> i32;

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    /// These symbols are private, so they are resolved at runtime: linking them directly would
    /// turn "the SDK no longer exports this" into a build failure for the whole lab.
    fn private_symbol(symbol: &CStr) -> Option<*mut c_void> {
        const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
        const RTLD_LAZY: c_int = 0x1;
        const SKYLIGHT_PATH: &CStr =
            c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";

        let mut address = unsafe { dlsym(RTLD_DEFAULT, symbol.as_ptr()) };
        if address.is_null() {
            let skylight = unsafe { dlopen(SKYLIGHT_PATH.as_ptr(), RTLD_LAZY) };
            if !skylight.is_null() {
                address = unsafe { dlsym(skylight, symbol.as_ptr()) };
            }
        }
        (!address.is_null()).then_some(address)
    }

    fn set_window_server_blur(native_window: id, radius: i32) -> SharedString {
        let Some(connection_symbol) = private_symbol(c"CGSDefaultConnectionForThread") else {
            return "CGSDefaultConnectionForThread not found".into();
        };
        let Some(blur_symbol) = private_symbol(c"CGSSetWindowBackgroundBlurRadius") else {
            return "CGSSetWindowBackgroundBlurRadius not found".into();
        };

        let window_number: NSInteger = unsafe { msg_send![native_window, windowNumber] };
        if window_number <= 0 {
            return "window has no window-server number yet".into();
        }

        let status = unsafe {
            let default_connection: DefaultConnectionForThread = mem::transmute(connection_symbol);
            let set_blur_radius: SetWindowBackgroundBlurRadius = mem::transmute(blur_symbol);
            set_blur_radius(default_connection(), window_number as u32, radius)
        };
        format!("CGSSetWindowBackgroundBlurRadius(radius={radius}) returned {status}").into()
    }
}

fn run_example() {
    application().run(|cx: &mut App| {
        for (index, (label, mechanism)) in CANDIDATES.iter().enumerate() {
            let column = index % GRID_COLUMNS;
            let row = index / GRID_COLUMNS;
            let bounds = Bounds {
                origin: point(
                    px(GRID_ORIGIN_X + (WINDOW_WIDTH + WINDOW_GAP) * column as f32),
                    px(GRID_ORIGIN_Y + (WINDOW_HEIGHT + WINDOW_GAP) * row as f32),
                ),
                size: size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
            };
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from(*label)),
                    ..Default::default()
                }),
                window_background: match mechanism {
                    Mechanism::GpuiBlurred => WindowBackgroundAppearance::Blurred,
                    _ => WindowBackgroundAppearance::Transparent,
                },
                ..Default::default()
            };

            let opened = cx.open_window(options, |_, cx| {
                cx.new(|_| Candidate {
                    label,
                    mechanism: *mechanism,
                    applied: false,
                    status: "applying…".into(),
                })
            });
            if let Err(error) = opened {
                eprintln!("failed to open the window for {label}: {error}");
            }
        }
        cx.activate(true);
    });
}

#[cfg(all(not(target_family = "wasm"), target_os = "macos"))]
fn main() {
    run_example();
}

#[cfg(all(not(target_family = "wasm"), not(target_os = "macos")))]
fn main() {
    eprintln!("window_blur_lab only demonstrates anything on macOS");
    run_example();
}
