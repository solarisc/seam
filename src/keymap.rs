//! Cross-platform keyboard translation before events enter the input transport.
use crate::{input::MODIFIER_KEYS, protocol::Input};
use rdev::Key;
use std::collections::HashSet;

/// Named OS conventions. Right Alt on Windows is excluded because it may be
/// AltGr, including the synthetic Ctrl transition used by international layouts.
#[derive(Clone, Copy)]
enum AppSwitch {
    WindowsToMac,
    MacToWindows,
}

impl AppSwitch {
    fn source(self, key: Key) -> bool {
        match self {
            Self::WindowsToMac => key == Key::Alt,
            Self::MacToWindows => matches!(key, Key::MetaLeft | Key::MetaRight),
        }
    }

    fn destination(self) -> Key {
        match self {
            Self::WindowsToMac => Key::MetaLeft,
            Self::MacToWindows => Key::Alt,
        }
    }
}

pub(crate) struct KeyboardMap {
    swap_command: bool,
    app_switch: Option<AppSwitch>,
    switching: bool,
    remote_modifiers: u16,
}

impl KeyboardMap {
    pub fn new(local: &str, remote: &str, command: bool, app_switch: bool) -> Self {
        let convention = match (local, remote) {
            ("windows", "macos") => Some(AppSwitch::WindowsToMac),
            ("macos", "windows") => Some(AppSwitch::MacToWindows),
            _ => None,
        };
        Self {
            swap_command: command && convention.is_some(),
            app_switch: convention.filter(|_| app_switch),
            switching: false,
            remote_modifiers: 0,
        }
    }

    /// Called after the old destination receives ReleaseAll. A new handoff
    /// carries physical modifiers, never a shortcut override from an old one.
    pub fn begin(&mut self, held: &HashSet<Key>, emit: impl FnMut(Input)) {
        self.switching = false;
        self.remote_modifiers = 0;
        self.synchronize(held, emit);
    }

    pub fn key(&mut self, key: Key, down: bool, held: &HashSet<Key>, mut emit: impl FnMut(Input)) {
        if let Some(convention) = self.app_switch {
            if !held.iter().any(|&key| convention.source(key)) {
                self.switching = false;
            } else if key == Key::Tab
                && down
                && MODIFIER_KEYS.iter().all(|&modifier| {
                    !held.contains(&modifier)
                        || convention.source(modifier)
                        || matches!(modifier, Key::ShiftLeft | Key::ShiftRight)
                })
            {
                self.switching = true;
            }
        }
        self.synchronize(held, &mut emit);
        if !MODIFIER_KEYS.contains(&key) {
            // Repeated key-downs remain repeated, in the original order.
            emit(Input::Key { key, down });
        }
    }

    fn synchronize(&mut self, held: &HashSet<Key>, mut emit: impl FnMut(Input)) {
        let mut desired = 0;
        for source in MODIFIER_KEYS {
            if !held.contains(&source) {
                continue;
            }
            let destination = if let Some(convention) = self.app_switch
                && self.switching
                && convention.source(source)
            {
                convention.destination()
            } else {
                translate_modifier(source, self.swap_command)
            };
            // Several physical modifiers can temporarily own one destination
            // modifier. Reconcile the whole set so either one's release cannot
            // release another owner's Command/Alt key.
            desired |= 1
                << MODIFIER_KEYS
                    .iter()
                    .position(|&key| key == destination)
                    .unwrap();
        }
        // Release the old interpretation before pressing the new modifier.
        for down in [false, true] {
            for (index, key) in MODIFIER_KEYS.into_iter().enumerate() {
                let bit = 1 << index;
                if (self.remote_modifiers ^ desired) & bit != 0 && (desired & bit != 0) == down {
                    emit(Input::Key { key, down });
                }
            }
        }
        self.remote_modifiers = desired;
    }
}

fn translate_modifier(key: Key, swap_command: bool) -> Key {
    if swap_command {
        // Swap both ways. Mapping only Ctrl to Command aliases the Windows
        // key to the same remote key, so releasing either breaks the other.
        match key {
            Key::ControlLeft => Key::MetaLeft,
            Key::ControlRight => Key::MetaRight,
            Key::MetaLeft => Key::ControlLeft,
            Key::MetaRight => Key::ControlRight,
            _ => key,
        }
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Trace {
        held: HashSet<Key>,
        output: Vec<(Key, bool)>,
    }

    impl Trace {
        fn key(&mut self, map: &mut KeyboardMap, key: Key, down: bool) {
            if down {
                self.held.insert(key);
            } else {
                self.held.remove(&key);
            }
            map.key(key, down, &self.held, |event| {
                self.output.push(key_event(event))
            });
        }

        fn begin(&mut self, map: &mut KeyboardMap) {
            map.begin(&self.held, |event| self.output.push(key_event(event)));
        }

        fn run(&mut self, map: &mut KeyboardMap, events: &[(Key, bool)]) {
            for &(key, down) in events {
                self.key(map, key, down);
            }
        }
    }

    fn key_event(event: Input) -> (Key, bool) {
        match event {
            Input::Key { key, down } => (key, down),
            _ => panic!("keyboard mapping emitted a non-key event"),
        }
    }

    #[test]
    fn shortcut_mapping_preserves_distinct_modifiers_and_round_trips() {
        // Preserve the existing side/identity contract at the production
        // event-output boundary rather than calling its private mapping helper.
        let pairs = [
            (Key::ControlLeft, Key::MetaLeft),
            (Key::ControlRight, Key::MetaRight),
            (Key::MetaLeft, Key::ControlLeft),
            (Key::MetaRight, Key::ControlRight),
            (Key::Alt, Key::Alt),
            (Key::AltGr, Key::AltGr),
            (Key::ShiftLeft, Key::ShiftLeft),
            (Key::ShiftRight, Key::ShiftRight),
            (Key::Function, Key::Function),
        ];
        for (local, remote) in [("windows", "macos"), ("macos", "windows")] {
            let mut map = KeyboardMap::new(local, remote, true, true);
            let mut trace = Trace::default();
            for &(source, destination) in &pairs {
                trace.key(&mut map, source, true);
                assert_eq!(trace.output.last(), Some(&(destination, true)));
            }
            assert_eq!(trace.output.len(), pairs.len());
            for &(source, destination) in &pairs {
                trace.key(&mut map, source, false);
                assert_eq!(trace.output.last(), Some(&(destination, false)));
            }
            assert_eq!(trace.output.len(), pairs.len() * 2);
        }
    }

    #[test]
    fn app_switch_keeps_destination_modifier_until_source_release() {
        for (local, remote, source, ordinary, destination) in [
            ("windows", "macos", Key::Alt, Key::Alt, Key::MetaLeft),
            (
                "macos",
                "windows",
                Key::MetaLeft,
                Key::ControlLeft,
                Key::Alt,
            ),
            (
                "macos",
                "windows",
                Key::MetaRight,
                Key::ControlRight,
                Key::Alt,
            ),
        ] {
            let mut map = KeyboardMap::new(local, remote, true, true);
            let mut trace = Trace::default();
            trace.run(
                &mut map,
                &[
                    (source, true),
                    (Key::Tab, true),
                    (Key::Tab, false),
                    (Key::ShiftLeft, true),
                    (Key::Tab, true),
                    (Key::Tab, true),
                    (Key::Tab, false),
                    (Key::ShiftLeft, false),
                    (source, false),
                    (Key::KeyA, true),
                    (Key::KeyA, false),
                ],
            );
            assert_eq!(
                trace.output,
                vec![
                    (ordinary, true),
                    (ordinary, false),
                    (destination, true),
                    (Key::Tab, true),
                    (Key::Tab, false),
                    (Key::ShiftLeft, true),
                    (Key::Tab, true),
                    (Key::Tab, true),
                    (Key::Tab, false),
                    (Key::ShiftLeft, false),
                    (destination, false),
                    (Key::KeyA, true),
                    (Key::KeyA, false),
                ],
                "{local} -> {remote}"
            );
        }
    }

    #[test]
    fn app_switch_setting_is_independent_and_nonmatching_chords_are_unchanged() {
        for (local, remote, enabled, keys) in [
            ("windows", "macos", false, vec![Key::Alt, Key::Tab]),
            ("macos", "windows", false, vec![Key::MetaLeft, Key::Tab]),
            ("windows", "windows", true, vec![Key::Alt, Key::Tab]),
            ("macos", "macos", true, vec![Key::MetaLeft, Key::Tab]),
            (
                "windows",
                "macos",
                true,
                vec![Key::ControlLeft, Key::AltGr, Key::Tab],
            ),
            ("windows", "macos", true, vec![Key::Alt, Key::KeyE]),
            (
                "windows",
                "macos",
                true,
                vec![Key::ControlLeft, Key::Alt, Key::Tab],
            ),
        ] {
            let mut map = KeyboardMap::new(local, remote, false, enabled);
            let mut trace = Trace::default();
            let sequence: Vec<_> = keys
                .iter()
                .map(|&key| (key, true))
                .chain(keys.iter().rev().map(|&key| (key, false)))
                .collect();
            trace.run(&mut map, &sequence);
            assert_eq!(
                trace.output, sequence,
                "{local} -> {remote}, app switching={enabled}"
            );
        }
        let mut map = KeyboardMap::new("windows", "macos", false, true);
        let mut trace = Trace::default();
        trace.run(
            &mut map,
            &[
                (Key::Alt, true),
                (Key::Tab, true),
                (Key::Tab, false),
                (Key::Alt, false),
            ],
        );
        assert_eq!(
            trace.output,
            vec![
                (Key::Alt, true),
                (Key::Alt, false),
                (Key::MetaLeft, true),
                (Key::Tab, true),
                (Key::Tab, false),
                (Key::MetaLeft, false)
            ]
        );
    }

    #[test]
    fn app_switch_respects_modifier_owners_and_restarts_from_physical_state() {
        let mut map = KeyboardMap::new("windows", "macos", true, true);
        let mut trace = Trace::default();
        // Alt is held before crossing. Tab opens the switcher, then Ctrl also
        // owns Command. Releasing Alt must leave Ctrl's Command held.
        trace.held.insert(Key::Alt);
        trace.begin(&mut map);
        trace.run(
            &mut map,
            &[
                (Key::Tab, true),
                (Key::Tab, false),
                (Key::ControlLeft, true),
                (Key::Alt, false),
                (Key::ControlLeft, false),
            ],
        );
        assert_eq!(
            trace.output,
            vec![
                (Key::Alt, true),
                (Key::Alt, false),
                (Key::MetaLeft, true),
                (Key::Tab, true),
                (Key::Tab, false),
                (Key::MetaLeft, false)
            ]
        );
        // An interrupted gesture is followed by ReleaseAll at the input owner.
        // A subsequent handoff must start with Option, not the old Command.
        trace.run(
            &mut map,
            &[(Key::Alt, true), (Key::Tab, true), (Key::Tab, false)],
        );
        trace.output.clear();
        trace.begin(&mut map);
        trace.run(
            &mut map,
            &[(Key::KeyE, true), (Key::KeyE, false), (Key::Alt, false)],
        );
        assert_eq!(
            trace.output,
            vec![
                (Key::Alt, true),
                (Key::KeyE, true),
                (Key::KeyE, false),
                (Key::Alt, false)
            ]
        );
    }
}
