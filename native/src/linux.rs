use anyhow::{Context, Result, bail};
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, uinput::VirtualDevice};
use serde_json::Value;
use std::{
    io::Write,
    process::{Command, Stdio},
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
    serde_json::from_slice(&output.stdout).ok()
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

pub fn paste(text: &str, target: &Option<Value>) -> Result<bool> {
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
    let Some(window) = focused_window() else {
        return Ok(false);
    };
    let Some(target) = target else {
        return Ok(false);
    };
    if window["id"] != target["id"] {
        return Ok(false);
    }
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
    // Check again after device registration before injecting keys.
    if focused_window().as_ref().map(|w| &w["id"]) != Some(&target["id"]) {
        return Ok(false);
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
    Ok(true)
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
