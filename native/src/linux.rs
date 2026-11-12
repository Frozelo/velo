use anyhow::{Context, Result, bail};
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, uinput::VirtualDevice};
use serde_json::Value;
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub fn focused_window() -> Option<Value> {
    let output = Command::new("niri")
        .args(["msg", "-j", "focused-window"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let window: Value = serde_json::from_slice(&output.stdout).ok()?;
    window.get("id")?.as_u64()?;
    Some(window)
}

pub fn listen(mut event: impl FnMut(bool)) -> Result<()> {
    let mut device = evdev::enumerate()
        .map(|(_, d)| d)
        .find(|d| d.name() == Some("keyd virtual keyboard"))
        .context(
            "No keyd virtual keyboard. This spike uses the existing Right Cmd → F24 mapping",
        )?;
    loop {
        for key in device.fetch_events()? {
            if key.event_type() == EventType::KEY && key.code() == KeyCode::KEY_F24.code() {
                match key.value() {
                    1 => event(true),
                    0 => event(false),
                    _ => {}
                }
            }
        }
    }
}

pub struct Delivery {
    pub pasted: bool,
    pub reason: &'static str,
    pub checked_windows: Vec<Option<u64>>,
}

fn window_id(window: &Option<Value>) -> Option<u64> {
    window.as_ref()?.get("id")?.as_u64()
}

fn denied(target: Option<u64>, current: Option<u64>) -> Option<&'static str> {
    match (target, current) {
        (None, _) => Some("target_window_unknown"),
        (_, None) => Some("current_window_unknown"),
        (Some(a), Some(b)) if a != b => Some("window_changed"),
        _ => None,
    }
}

pub fn copy_text(text: &str) -> Result<()> {
    let mut copy = Command::new("wl-copy")
        .arg("--")
        .stdin(Stdio::piped())
        .spawn()?;
    copy.stdin
        .take()
        .context("Clipboard stdin")?
        .write_all(text.as_bytes())?;
    if !copy.wait()?.success() {
        bail!("Clipboard copy failed")
    }
    Ok(())
}

pub fn paste(text: &str, target: &Option<Value>, cancelled: &AtomicBool) -> Result<Delivery> {
    let mut delivery = Delivery {
        pasted: false,
        reason: "cancelled",
        checked_windows: Vec::new(),
    };
    if cancelled.load(Ordering::Relaxed) {
        return Ok(delivery);
    }
    copy_text(text)?;
    let window = focused_window();
    let target_id = window_id(target);
    delivery.checked_windows.push(window_id(&window));
    if let Some(reason) = denied(target_id, window_id(&window)) {
        delivery.reason = reason;
        return Ok(delivery);
    }
    let window = window.unwrap();
    let terminal = matches!(
        window["app_id"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .as_str(),
        "kitty" | "alacritty" | "com.mitchellh.ghostty" | "org.gnome.terminal"
    );
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::KEY_LEFTCTRL);
    keys.insert(KeyCode::KEY_V);
    if terminal {
        keys.insert(KeyCode::KEY_LEFTSHIFT);
    }
    let mut keyboard = VirtualDevice::builder()?
        .name("Velo native paste")
        .with_keys(&keys)?
        .build()?;
    std::thread::sleep(Duration::from_millis(150));
    // Record the exact decision point, not a separate earlier snapshot.
    let current_id = window_id(&focused_window());
    delivery.checked_windows.push(current_id);
    if let Some(reason) = denied(target_id, current_id) {
        delivery.reason = reason;
        return Ok(delivery);
    }
    if cancelled.load(Ordering::Relaxed) {
        return Ok(delivery);
    }
    let mut pressed = vec![KeyCode::KEY_LEFTCTRL];
    if terminal {
        pressed.push(KeyCode::KEY_LEFTSHIFT);
    }
    pressed.push(KeyCode::KEY_V);
    let mut events: Vec<_> = pressed
        .iter()
        .map(|k| InputEvent::new(EventType::KEY.0, k.code(), 1))
        .collect();
    events.extend(
        pressed
            .iter()
            .rev()
            .map(|k| InputEvent::new(EventType::KEY.0, k.code(), 0)),
    );
    keyboard.emit(&events)?;
    delivery.pasted = true;
    delivery.reason = "keys_sent";
    Ok(delivery)
}

pub fn check_input() -> Result<()> {
    let device: Device = evdev::enumerate()
        .map(|(_, d)| d)
        .find(|d| d.name() == Some("keyd virtual keyboard"))
        .context("keyd virtual keyboard not readable")?;
    println!("Keyboard: {}", device.name().unwrap_or("unknown"));
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .context("No access to /dev/uinput")?;
    println!("uinput: writable");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn focus_decision_explains_unknown_changed_and_same_windows() {
        assert_eq!(super::denied(None, None), Some("target_window_unknown"));
        assert_eq!(
            super::denied(Some(20), None),
            Some("current_window_unknown")
        );
        assert_eq!(super::denied(Some(20), Some(21)), Some("window_changed"));
        assert_eq!(super::denied(Some(20), Some(20)), None);
    }
    #[test]
    fn pre_cancelled_delivery_does_not_touch_clipboard_or_keyboard() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let result = super::paste("do not paste", &None, &cancelled).unwrap();
        assert!(!result.pasted);
        assert_eq!(result.reason, "cancelled");
        assert!(result.checked_windows.is_empty());
    }
}
