//! **PLATFORM GLUE (iPhone)**, not in the original: the game's force feedback on the phone's own vibration
//! motor (the Taptic Engine), through CoreHaptics.
//!
//! The force feedback is mapped onto a pad's two rumble motors (PORT-DEVIATION D24, `app::force_feedback`), and
//! `nfs_input::ff::Rumbler` sends both motors' levels in one call. With no connected pad that can rumble, that call
//! comes here ([`nfs_input::gamepad::Gamepads::set_rumble_fallback`]): one looping continuous haptic event whose
//! intensity and sharpness follow the two motors — the strong (road, collisions) motor as a dull thump, the weak
//! (engine) one as a sharper buzz. A pad that can rumble always takes the force feedback instead.
//!
//! An iPad has no Taptic Engine (`supportsHaptics` is false), so nothing is set up there. The engine stops itself
//! when the app goes to the background or the audio session is interrupted; a refused call starts it again.

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use objc2_foundation::{NSArray, NSString};

#[link(name = "CoreHaptics", kind = "framework")]
unsafe extern "C" {
    static CHHapticEventTypeHapticContinuous: &'static NSString;
    static CHHapticEventParameterIDHapticIntensity: &'static NSString;
    static CHHapticEventParameterIDHapticSharpness: &'static NSString;
    static CHHapticDynamicParameterIDHapticIntensityControl: &'static NSString;
    static CHHapticDynamicParameterIDHapticSharpnessControl: &'static NSString;
}

/// The longest a continuous haptic event may last (CoreHaptics' limit); the player loops it.
const EVENT_SECONDS: f64 = 30.0;

/// The phone's vibration motor, driven by the two rumble levels.
pub(crate) struct PhoneRumble {
    engine: Retained<AnyObject>,
    player: Option<Retained<AnyObject>>,
    playing: bool,
}

/// Whether this device has a Taptic Engine CoreHaptics can drive.
pub(crate) fn supported() -> bool {
    let Some(cls) = AnyClass::get(c"CHHapticEngine") else { return false };
    // SAFETY: CoreHaptics' documented class method and property
    unsafe {
        let caps: Option<Retained<AnyObject>> = msg_send![cls, capabilitiesForHardware];
        caps.is_some_and(|c| {
            let b: Bool = msg_send![&*c, supportsHaptics];
            b.as_bool()
        })
    }
}

/// The intensity and sharpness (0…1) of the two motors' levels (0…65535) together.
pub(crate) fn feel(low: u16, high: u16) -> (f32, f32) {
    let (l, h) = (f32::from(low) / 65535.0, f32::from(high) / 65535.0);
    if l <= 0.0 && h <= 0.0 {
        return (0.0, 0.0);
    }
    let intensity = (l + 0.5 * h).min(1.0);
    let sharpness = 0.1 + 0.5 * h / (l + h);
    (intensity, sharpness)
}

impl PhoneRumble {
    /// The haptic engine, or `None` on a device without a Taptic Engine.
    pub(crate) fn new() -> Option<Self> {
        if !supported() {
            return None;
        }
        let cls = AnyClass::get(c"CHHapticEngine")?;
        // SAFETY: CoreHaptics' documented API
        unsafe {
            let alloc: Allocated<AnyObject> = msg_send![cls, alloc];
            let engine: Option<Retained<AnyObject>> = msg_send![alloc, initAndReturnError: std::ptr::null_mut::<*mut AnyObject>()];
            let engine = engine?;
            // haptics only: the game's sound does not go through it, so the engine leaves the audio session alone
            let _: () = msg_send![&*engine, setPlaysHapticsOnly: true];
            let _: () = msg_send![&*engine, setAutoShutdownEnabled: false];
            Some(Self { engine, player: None, playing: false })
        }
    }

    /// (Re)start the engine and make the looping player.
    fn start(&mut self) -> bool {
        // SAFETY: CoreHaptics' documented API; the parameter names are its own constants
        unsafe {
            let ok: Bool = msg_send![&*self.engine, startAndReturnError: std::ptr::null_mut::<*mut AnyObject>()];
            if !ok.as_bool() {
                return false;
            }
            let Some(pcls) = AnyClass::get(c"CHHapticEventParameter") else { return false };
            let Some(ecls) = AnyClass::get(c"CHHapticEvent") else { return false };
            let Some(patcls) = AnyClass::get(c"CHHapticPattern") else { return false };
            let param = |id: &NSString, v: f32| -> Option<Retained<AnyObject>> {
                let a: Allocated<AnyObject> = msg_send![pcls, alloc];
                msg_send![a, initWithParameterID: id, value: v]
            };
            let (Some(i), Some(s)) = (param(CHHapticEventParameterIDHapticIntensity, 1.0), param(CHHapticEventParameterIDHapticSharpness, 0.5)) else {
                return false;
            };
            let params = NSArray::from_retained_slice(&[i, s]);
            let a: Allocated<AnyObject> = msg_send![ecls, alloc];
            let event: Option<Retained<AnyObject>> =
                msg_send![a, initWithEventType: CHHapticEventTypeHapticContinuous, parameters: &*params, relativeTime: 0.0f64, duration: EVENT_SECONDS];
            let Some(event) = event else { return false };
            let events = NSArray::from_retained_slice(&[event]);
            let none: Retained<NSArray<AnyObject>> = NSArray::new();
            let a: Allocated<AnyObject> = msg_send![patcls, alloc];
            let pattern: Option<Retained<AnyObject>> =
                msg_send![a, initWithEvents: &*events, parameters: &*none, error: std::ptr::null_mut::<*mut AnyObject>()];
            let Some(pattern) = pattern else { return false };
            let player: Option<Retained<AnyObject>> =
                msg_send![&*self.engine, createAdvancedPlayerWithPattern: &*pattern, error: std::ptr::null_mut::<*mut AnyObject>()];
            let Some(player) = player else { return false };
            let _: () = msg_send![&*player, setLoopEnabled: true];
            self.player = Some(player);
            self.playing = false;
            true
        }
    }

    /// The rumble call (`nfs_input::gamepad::RumbleFallback`): both motors' levels; 0, 0 stops. False when
    /// CoreHaptics refused it (the rumbler then waits before the next try).
    pub(crate) fn rumble(&mut self, low: u16, high: u16, _ms: u32) -> bool {
        let (intensity, sharpness) = feel(low, high);
        if intensity <= 0.0 {
            if self.playing
                && let Some(p) = &self.player
            {
                // SAFETY: CoreHaptics' documented API
                let _: Bool = unsafe { msg_send![&**p, stopAtTime: 0.0f64, error: std::ptr::null_mut::<*mut AnyObject>()] };
            }
            self.playing = false;
            return true;
        }
        if self.send(intensity, sharpness) {
            return true;
        }
        // the engine stopped (background, an audio interruption): start it again once
        self.player = None;
        self.start() && self.send(intensity, sharpness)
    }

    fn send(&mut self, intensity: f32, sharpness: f32) -> bool {
        if self.player.is_none() && !self.start() {
            return false;
        }
        let Some(p) = &self.player else { return false };
        let Some(dcls) = AnyClass::get(c"CHHapticDynamicParameter") else { return false };
        // SAFETY: CoreHaptics' documented API; the parameter names are its own constants
        unsafe {
            let dyn_param = |id: &NSString, v: f32| -> Option<Retained<AnyObject>> {
                let a: Allocated<AnyObject> = msg_send![dcls, alloc];
                msg_send![a, initWithParameterID: id, value: v, relativeTime: 0.0f64]
            };
            let (Some(i), Some(s)) = (
                dyn_param(CHHapticDynamicParameterIDHapticIntensityControl, intensity),
                dyn_param(CHHapticDynamicParameterIDHapticSharpnessControl, sharpness - 0.5),
            ) else {
                return false;
            };
            let params = NSArray::from_retained_slice(&[i, s]);
            let ok: Bool = msg_send![&**p, sendParameters: &*params, atTime: 0.0f64, error: std::ptr::null_mut::<*mut AnyObject>()];
            if !ok.as_bool() {
                return false;
            }
            if !self.playing {
                let ok: Bool = msg_send![&**p, startAtTime: 0.0f64, error: std::ptr::null_mut::<*mut AnyObject>()];
                if !ok.as_bool() {
                    return false;
                }
                self.playing = true;
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::feel;

    #[test]
    fn feel_follows_the_motors() {
        assert_eq!(feel(0, 0), (0.0, 0.0));
        let (i, s) = feel(u16::MAX, 0);
        assert!((i - 1.0).abs() < 1e-6 && (s - 0.1).abs() < 1e-6);
        let (i, s) = feel(0, u16::MAX);
        assert!((i - 0.5).abs() < 1e-6 && (s - 0.6).abs() < 1e-6);
        assert!(feel(u16::MAX, u16::MAX).0 <= 1.0);
    }
}
