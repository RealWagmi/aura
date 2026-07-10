use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SPACE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

#[derive(Debug)]
pub struct PushToTalkWatcher {
    pub presses: Arc<AtomicU64>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hotkey {
    keys: Vec<KeyGroup>,
    modifiers: ModifierMask,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyGroup {
    keys: Vec<i32>,
}

impl KeyGroup {
    fn one(key: i32) -> Self {
        Self { keys: vec![key] }
    }

    fn any(keys: impl Into<Vec<i32>>) -> Self {
        Self { keys: keys.into() }
    }

    fn is_down(&self) -> bool {
        self.keys.iter().any(|key| is_key_down(*key))
    }
}

type ModifierMask = u8;
const MOD_CTRL: ModifierMask = 1 << 0;
const MOD_ALT: ModifierMask = 1 << 1;
const MOD_SHIFT: ModifierMask = 1 << 2;
const MOD_WIN: ModifierMask = 1 << 3;

pub fn start_push_to_talk_watcher(
    raw: &str,
    allow_global: bool,
) -> Result<PushToTalkWatcher, String> {
    let hotkey = parse_hotkey(raw)?;
    if !allow_global && unsafe { GetConsoleWindow() }.is_null() {
        return Err(
            "focus-scoped push-to-talk needs a console window; set AURA_PUSH_TO_TALK_ALLOW_GLOBAL_HOTKEY=1 only if you explicitly accept a system-wide hotkey"
                .to_owned(),
        );
    }
    let label = hotkey.label.clone();
    let presses = Arc::new(AtomicU64::new(0));
    let watcher_presses = Arc::clone(&presses);

    std::thread::spawn(move || {
        let mut was_pressed = false;
        loop {
            let in_scope =
                allow_global || unsafe { GetForegroundWindow() } == unsafe { GetConsoleWindow() };
            let pressed = in_scope && hotkey.is_pressed_exact();
            if pressed && !was_pressed {
                watcher_presses.fetch_add(1, Ordering::AcqRel);
            }
            was_pressed = pressed;
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    Ok(PushToTalkWatcher { presses, label })
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
    let mut modifiers = 0;
    let mut has_action_key = false;
    let mut labels = Vec::with_capacity(parts.len());
    for part in parts {
        let (key, label, modifier) = parse_key(part)?;
        if !keys.contains(&key) {
            keys.push(key);
            labels.push(label);
        }
        if let Some(modifier) = modifier {
            modifiers |= modifier;
        } else {
            has_action_key = true;
        }
    }

    if modifiers == 0 || !has_action_key {
        return Err(
            "AURA_PUSH_TO_TALK_HOTKEY must combine a modifier with an action key (for example ctrl+space or alt+a)"
                .to_owned(),
        );
    }

    Ok(Hotkey {
        keys,
        modifiers,
        label: labels.join("+"),
    })
}

fn parse_key(raw: &str) -> Result<(KeyGroup, String, Option<ModifierMask>), String> {
    let normalized = raw.trim().to_ascii_lowercase();
    let key = match normalized.as_str() {
        "ctrl" | "control" => (
            KeyGroup::one(VK_CONTROL as i32),
            "Ctrl".to_owned(),
            Some(MOD_CTRL),
        ),
        "lctrl" | "leftctrl" | "left_control" => (
            KeyGroup::one(VK_LCONTROL as i32),
            "Left Ctrl".to_owned(),
            Some(MOD_CTRL),
        ),
        "rctrl" | "rightctrl" | "right_control" => (
            KeyGroup::one(VK_RCONTROL as i32),
            "Right Ctrl".to_owned(),
            Some(MOD_CTRL),
        ),
        "alt" => (
            KeyGroup::one(VK_MENU as i32),
            "Alt".to_owned(),
            Some(MOD_ALT),
        ),
        "lalt" | "leftalt" | "left_alt" => (
            KeyGroup::one(VK_LMENU as i32),
            "Left Alt".to_owned(),
            Some(MOD_ALT),
        ),
        "ralt" | "rightalt" | "right_alt" => (
            KeyGroup::one(VK_RMENU as i32),
            "Right Alt".to_owned(),
            Some(MOD_ALT),
        ),
        "shift" => (
            KeyGroup::one(VK_SHIFT as i32),
            "Shift".to_owned(),
            Some(MOD_SHIFT),
        ),
        "lshift" | "leftshift" | "left_shift" => (
            KeyGroup::one(VK_LSHIFT as i32),
            "Left Shift".to_owned(),
            Some(MOD_SHIFT),
        ),
        "rshift" | "rightshift" | "right_shift" => (
            KeyGroup::one(VK_RSHIFT as i32),
            "Right Shift".to_owned(),
            Some(MOD_SHIFT),
        ),
        "win" | "windows" | "meta" => (
            KeyGroup::any([VK_LWIN as i32, VK_RWIN as i32]),
            "Win".to_owned(),
            Some(MOD_WIN),
        ),
        "lwin" | "leftwin" | "left_win" => (
            KeyGroup::one(VK_LWIN as i32),
            "Left Win".to_owned(),
            Some(MOD_WIN),
        ),
        "rwin" | "rightwin" | "right_win" => (
            KeyGroup::one(VK_RWIN as i32),
            "Right Win".to_owned(),
            Some(MOD_WIN),
        ),
        "space" => (KeyGroup::one(VK_SPACE as i32), "Space".to_owned(), None),
        key if key.len() == 1 => {
            let ch = key.chars().next().expect("single-char key");
            if ch.is_ascii_alphanumeric() {
                (
                    KeyGroup::one(ch.to_ascii_uppercase() as i32),
                    ch.to_ascii_uppercase().to_string(),
                    None,
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

impl Hotkey {
    fn is_pressed_exact(&self) -> bool {
        self.keys.iter().all(KeyGroup::is_down)
            && modifier_is_exact(
                self.modifiers,
                MOD_CTRL,
                [VK_LCONTROL as i32, VK_RCONTROL as i32],
            )
            && modifier_is_exact(self.modifiers, MOD_ALT, [VK_LMENU as i32, VK_RMENU as i32])
            && modifier_is_exact(
                self.modifiers,
                MOD_SHIFT,
                [VK_LSHIFT as i32, VK_RSHIFT as i32],
            )
            && modifier_is_exact(self.modifiers, MOD_WIN, [VK_LWIN as i32, VK_RWIN as i32])
    }
}

fn modifier_is_exact(required: ModifierMask, modifier: ModifierMask, keys: [i32; 2]) -> bool {
    let required = (required & modifier) != 0;
    let down = keys.iter().any(|key| is_key_down(*key));
    required == down
}

#[cfg(test)]
mod tests {
    use super::{parse_hotkey, MOD_ALT, MOD_CTRL, MOD_SHIFT};

    #[test]
    fn parses_default_hotkey() {
        let hotkey = parse_hotkey("ctrl+space").expect("hotkey");
        assert_eq!(hotkey.label, "Ctrl+Space");
        assert_eq!(hotkey.keys.len(), 2);
        assert_eq!(hotkey.modifiers, MOD_CTRL);
    }

    #[test]
    fn parses_letter_hotkey() {
        let hotkey = parse_hotkey("alt+a").expect("hotkey");
        assert_eq!(hotkey.label, "Alt+A");
        assert_eq!(hotkey.modifiers, MOD_ALT);
    }

    #[test]
    fn tracks_exact_modifier_families() {
        let hotkey = parse_hotkey("ctrl+shift+space").expect("hotkey");
        assert_eq!(hotkey.modifiers, MOD_CTRL | MOD_SHIFT);
    }

    #[test]
    fn generic_win_accepts_either_side() {
        let hotkey = parse_hotkey("win+space").expect("hotkey");
        assert_eq!(hotkey.label, "Win+Space");
        assert_eq!(hotkey.keys[0].keys.len(), 2);
    }

    #[test]
    fn rejects_unknown_key() {
        assert!(parse_hotkey("ctrl+nope").is_err());
    }

    #[test]
    fn rejects_modifier_free_letters_and_numbers() {
        assert!(parse_hotkey("a").is_err());
        assert!(parse_hotkey("7").is_err());
        assert!(parse_hotkey("space").is_err());
        assert!(parse_hotkey("ctrl").is_err());
        assert!(parse_hotkey("ctrl+shift").is_err());
        assert!(parse_hotkey("a+a").is_err());
    }
}
