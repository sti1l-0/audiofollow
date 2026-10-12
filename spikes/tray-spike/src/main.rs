//! Tray-icon feasibility + footprint spike.
//!
//! Answers two questions concretely:
//!   1. Can a Rust app register a Windows notification-area (tray) icon and menu?
//!   2. What does that actually cost in binary size and resident memory?
//!
//! It self-terminates after ~12s so it is safe to run unattended.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::{Duration, Instant};

use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

// ------------------------------------------------------------ memory probe --

#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn K32GetProcessMemoryInfo(
        h_process: *mut c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}

fn rss_kb() -> f64 {
    let mut c = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    if ok == 0 {
        return f64::NAN;
    }
    c.working_set_size as f64 / 1024.0
}

// ---------------------------------------------------------------- the spike --

fn make_icon(r: u8, g: u8, b: u8) -> Icon {
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            // simple filled circle with a transparent corner
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let inside = (dx * dx + dy * dy).sqrt() < 14.0;
            if inside {
                rgba.extend_from_slice(&[r, g, b, 255]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, size, size).expect("icon")
}

fn main() {
    println!("=== AudioFollow tray spike ===");
    println!("pid = {}", std::process::id());
    println!("rss at start          : {:.1} MB", rss_kb() / 1024.0);

    let event_loop = EventLoopBuilder::new().build();

    let menu = Menu::new();
    let title = MenuItem::new("音频跟随 (spike)", false, None);
    let quit = MenuItem::new("退出", true, None);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    quit.set_enabled(true);
    menu.append(&title).unwrap();
    menu.append(&PredefinedMenuItem::separator()).unwrap();
    menu.append(&quit).unwrap();

    let tray = TrayIconBuilder::new()
        .with_tooltip("AudioFollow spike — 按屏幕路由音频")
        .with_icon(make_icon(0xD8, 0x5A, 0x30))
        .with_menu(Box::new(menu))
        .build();

    match tray {
        Ok(t) => {
            println!("Shell_NotifyIcon      : OK (tray icon registered)");
            println!("rss after tray created: {:.1} MB", rss_kb() / 1024.0);
            // keep it alive for the whole run
            std::mem::forget(t);
        }
        Err(e) => {
            println!("Shell_NotifyIcon      : FAILED -> {e}");
            return;
        }
    }

    let started = Instant::now();
    let mut printed_mid = false;
    let mut stopped = false;
    let _ = tx;

    event_loop.run(move |_event, _, control_flow| {
        let elapsed = started.elapsed();
        if !printed_mid && elapsed > Duration::from_secs(4) {
            printed_mid = true;
            println!("rss at 4s (idle)      : {:.1} MB", rss_kb() / 1024.0);
        }
        if !stopped && elapsed > Duration::from_secs(12) {
            stopped = true;
            println!("rss at exit           : {:.1} MB", rss_kb() / 1024.0);
            *control_flow = ControlFlow::Exit;
            return;
        }
        *control_flow = ControlFlow::WaitUntil(started + elapsed + Duration::from_millis(250));
        let _ = rx.try_recv();
    });
}
