use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SPACE,
};

#[derive(Debug)]
pub struct PushToTalkWatcher {
    pub presses: Arc<AtomicU64>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hotkey {
    keys: Vec<i32>,
    label: String,
}

pub fn start_push_to_talk_watcher(raw: &str) -> Result<PushToTalkWatcher, String> {
    let hotkey = parse_hotkey(raw)?;
    let presses = Arc::new(AtomicU64::new(0));
    let watcher_presses = Arc::clone(&presses);
    let keys = hotkey.keys.clone();

    std::thread::spawn(move || {
        let mut was_pressed = false;
        loop {
            let pressed = keys.iter().all(|key| is_key_down(*key));
            if pressed && !was_pressed {
                watcher_presses.fetch_add(1, Ordering::AcqRel);
            }
            was_pressed = pressed;
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    Ok(PushToTalkWatcher {
        presses,
        label: hotkey.label,
    })
}

fn parse_hotkey(raw: &str) -> Result<Hotkey, String> {
    let parts: Vec<&str> = raw
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("AURA_PUSH_TO_TALK_HOTKEY must not be empty".to_owned());
    }

    let mut keys = Vec::with_capacity(parts.len());
    let mut labels = Vec::with_capacity(parts.len());
    for part in parts {
        let (key, label) = parse_key(part)?;
        if !keys.contains(&key) {
            keys.push(key);
            labels.push(label);
        }
    }

    Ok(Hotkey {
        keys,
        label: labels.join("+"),
    })
}

fn parse_key(raw: &str) -> Result<(i32, String), String> {
    let normalized = raw.trim().to_ascii_lowercase();
    let key = match normalized.as_str() {
        "ctrl" | "control" => (VK_CONTROL as i32, "Ctrl".to_owned()),
        "lctrl" | "leftctrl" | "left_control" => (VK_LCONTROL as i32, "Left Ctrl".to_owned()),
        "rctrl" | "rightctrl" | "right_control" => (VK_RCONTROL as i32, "Right Ctrl".to_owned()),
        "alt" => (VK_MENU as i32, "Alt".to_owned()),
        "lalt" | "leftalt" | "left_alt" => (VK_LMENU as i32, "Left Alt".to_owned()),
        "ralt" | "rightalt" | "right_alt" => (VK_RMENU as i32, "Right Alt".to_owned()),
        "shift" => (VK_SHIFT as i32, "Shift".to_owned()),
        "lshift" | "leftshift" | "left_shift" => (VK_LSHIFT as i32, "Left Shift".to_owned()),
        "rshift" | "rightshift" | "right_shift" => (VK_RSHIFT as i32, "Right Shift".to_owned()),
        "win" | "windows" | "meta" => (VK_LWIN as i32, "Win".to_owned()),
        "lwin" | "leftwin" | "left_win" => (VK_LWIN as i32, "Left Win".to_owned()),
        "rwin" | "rightwin" | "right_win" => (VK_RWIN as i32, "Right Win".to_owned()),
        "space" => (VK_SPACE as i32, "Space".to_owned()),
        key if key.len() == 1 => {
            let ch = key.chars().next().expect("single-char key");
            if ch.is_ascii_alphanumeric() {
                (
                    ch.to_ascii_uppercase() as i32,
                    ch.to_ascii_uppercase().to_string(),
                )
            } else {
                return Err(format!("unsupported hotkey key {raw:?}"));
            }
        }
        _ => return Err(format!("unsupported hotkey key {raw:?}")),
    };
    Ok(key)
}

fn is_key_down(key: i32) -> bool {
    unsafe { (GetAsyncKeyState(key) & 0x8000u16 as i16) != 0 }
}

#[cfg(test)]
mod tests {
    use super::parse_hotkey;

    #[test]
    fn parses_default_hotkey() {
        let hotkey = parse_hotkey("ctrl+space").expect("hotkey");
        assert_eq!(hotkey.label, "Ctrl+Space");
        assert_eq!(hotkey.keys.len(), 2);
    }

    #[test]
    fn parses_letter_hotkey() {
        let hotkey = parse_hotkey("alt+a").expect("hotkey");
        assert_eq!(hotkey.label, "Alt+A");
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(parse_hotkey("ctrl+nope").is_err());
    }
}
