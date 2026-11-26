//! OpenXR-backed [`TrackingSource`]: head and controllers sampled **at** the given time (no
//! prediction), plus controller input and haptics.

use openlink_client::TrackingSource;
use openlink_protocol::{
    buttons, Batteries, ControllerInput, ControllerState, HapticPulse, HeadsetInfo, Motion, MotionFlags, Pose,
    TrackingSample,
};
use openxr as xr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

const HANDS: [&str; 2] = ["/user/hand/left", "/user/hand/right"];

/// How far ahead (microseconds) the head is sampled: SteamVR does not predict the head (the
/// driver hands it over without velocity), so the headset's runtime predicts it to when the
/// frame rendered from it will be on screen. Tuned continuously by the render loop from the
/// measured prediction lead.
pub static HEAD_HORIZON_US: AtomicI32 = AtomicI32::new(40_000);

/// The action set; created before the session's action sets are attached.
pub struct Actions {
    pub set: xr::ActionSet,
    grip: xr::Action<xr::Posef>,
    ax_click: xr::Action<bool>,
    ax_touch: xr::Action<bool>,
    by_click: xr::Action<bool>,
    by_touch: xr::Action<bool>,
    menu_click: xr::Action<bool>,
    stick_click: xr::Action<bool>,
    stick_touch: xr::Action<bool>,
    thumbrest_touch: xr::Action<bool>,
    trigger_touch: xr::Action<bool>,
    trigger: xr::Action<f32>,
    squeeze: xr::Action<f32>,
    stick: xr::Action<xr::Vector2f>,
    haptic: xr::Action<xr::Haptic>,
    hands: [xr::Path; 2],
}

impl Actions {
    pub fn new(instance: &xr::Instance) -> xr::Result<Self> {
        let set = instance.create_action_set("openlink", "OpenLink", 0)?;
        let hands = [instance.string_to_path(HANDS[0])?, instance.string_to_path(HANDS[1])?];
        macro_rules! action {
            ($name:literal, $label:literal) => {
                set.create_action($name, $label, &hands)?
            };
        }
        let a = Self {
            grip: action!("grip_pose", "Grip pose"),
            ax_click: action!("ax_click", "A/X"),
            ax_touch: action!("ax_touch", "A/X touch"),
            by_click: action!("by_click", "B/Y"),
            by_touch: action!("by_touch", "B/Y touch"),
            menu_click: action!("menu_click", "Menu"),
            stick_click: action!("stick_click", "Thumbstick click"),
            stick_touch: action!("stick_touch", "Thumbstick touch"),
            thumbrest_touch: action!("thumbrest_touch", "Thumbrest touch"),
            trigger_touch: action!("trigger_touch", "Trigger touch"),
            trigger: action!("trigger", "Trigger"),
            squeeze: action!("squeeze", "Grip"),
            stick: action!("stick", "Thumbstick"),
            haptic: action!("haptic", "Haptics"),
            set,
            hands,
        };
        let p = |s: &str| instance.string_to_path(s);
        let mut b = Vec::new();
        for (i, hand) in HANDS.iter().enumerate() {
            let (ax, by) = if i == 0 { ("x", "y") } else { ("a", "b") };
            b.push(xr::Binding::new(&a.grip, p(&format!("{hand}/input/grip/pose"))?));
            b.push(xr::Binding::new(&a.ax_click, p(&format!("{hand}/input/{ax}/click"))?));
            b.push(xr::Binding::new(&a.ax_touch, p(&format!("{hand}/input/{ax}/touch"))?));
            b.push(xr::Binding::new(&a.by_click, p(&format!("{hand}/input/{by}/click"))?));
            b.push(xr::Binding::new(&a.by_touch, p(&format!("{hand}/input/{by}/touch"))?));
            b.push(xr::Binding::new(&a.stick_click, p(&format!("{hand}/input/thumbstick/click"))?));
            b.push(xr::Binding::new(&a.stick_touch, p(&format!("{hand}/input/thumbstick/touch"))?));
            b.push(xr::Binding::new(&a.thumbrest_touch, p(&format!("{hand}/input/thumbrest/touch"))?));
            b.push(xr::Binding::new(&a.trigger_touch, p(&format!("{hand}/input/trigger/touch"))?));
            b.push(xr::Binding::new(&a.trigger, p(&format!("{hand}/input/trigger/value"))?));
            b.push(xr::Binding::new(&a.squeeze, p(&format!("{hand}/input/squeeze/value"))?));
            b.push(xr::Binding::new(&a.stick, p(&format!("{hand}/input/thumbstick"))?));
            b.push(xr::Binding::new(&a.haptic, p(&format!("{hand}/output/haptic"))?));
        }
        // Only the left controller has a bindable menu button (the right one is the system button).
        b.push(xr::Binding::new(&a.menu_click, p("/user/hand/left/input/menu/click")?));
        instance.suggest_interaction_profile_bindings(p("/interaction_profiles/oculus/touch_controller")?, &b)?;
        Ok(a)
    }
}

impl Actions {
    pub fn grip_space<G: xr::Graphics>(&self, session: &xr::Session<G>, hand: usize) -> xr::Result<xr::Space> {
        self.grip.create_space(session, self.hands[hand], xr::Posef::IDENTITY)
    }
}

pub struct XrTracking {
    pub instance: xr::Instance,
    pub session: xr::Session<xr::AnyGraphics>,
    pub actions: Actions,
    pub stage: xr::Space,
    pub view: xr::Space,
    pub grips: [xr::Space; 2],
    pub info: HeadsetInfo,
    /// The last head motion with a valid pose, repeated (without velocity) when OpenXR cannot
    /// locate the head, so no identity pose is ever sent.
    pub last_hmd: Option<Motion>,
    /// Head samples OpenXR could not locate since the last log, and when that was.
    pub lost_hmd: (u32, Option<std::time::Instant>),
    /// For reading the headset battery through JNI.
    pub app: android_activity::AndroidApp,
}

impl XrTracking {
    /// `CLOCK_MONOTONIC` nanoseconds -> `XrTime`.
    fn xr_time(&self, ns: i64) -> xr::Time {
        let ts = libc::timespec { tv_sec: ns / 1_000_000_000, tv_nsec: ns % 1_000_000_000 };
        let mut out = xr::Time::from_nanos(ns);
        if let Some(ext) = self.instance.exts().khr_convert_timespec_time {
            // SAFETY: valid instance and pointers.
            unsafe { (ext.convert_timespec_time_to_time)(self.instance.as_raw(), &ts, &mut out) };
        }
        out
    }

    fn controller(&self, hand: usize, t: xr::Time) -> Option<ControllerState> {
        let s = &self.session;
        let path = self.actions.hands[hand];
        let a = &self.actions;
        if !a.grip.is_active(s, path).unwrap_or(false) {
            return None;
        }
        let motion = relate(&self.grips[hand], &self.stage, t);
        let b = |action: &xr::Action<bool>, bit: u32| {
            if action.state(s, path).map(|st| st.current_state).unwrap_or(false) {
                bit
            } else {
                0
            }
        };
        let f = |action: &xr::Action<f32>| action.state(s, path).map(|st| st.current_state).unwrap_or(0.0);
        let stick = a.stick.state(s, path).map(|st| st.current_state).unwrap_or_default();
        let grip = f(&a.squeeze);
        let input = ControllerInput {
            buttons: b(&a.ax_click, buttons::A_X_CLICK)
                | b(&a.ax_touch, buttons::A_X_TOUCH)
                | b(&a.by_click, buttons::B_Y_CLICK)
                | b(&a.by_touch, buttons::B_Y_TOUCH)
                | b(&a.menu_click, buttons::MENU_CLICK)
                | b(&a.stick_click, buttons::THUMBSTICK_CLICK)
                | b(&a.stick_touch, buttons::THUMBSTICK_TOUCH)
                | b(&a.thumbrest_touch, buttons::THUMBREST_TOUCH)
                | b(&a.trigger_touch, buttons::TRIGGER_TOUCH)
                // Touch has no grip sensor; any pressure counts as touching.
                | if grip > 0.05 { buttons::GRIP_TOUCH } else { 0 },
            trigger: f(&a.trigger),
            grip,
            thumbstick: [stick.x, stick.y],
        };
        Some(ControllerState { motion, input })
    }
}

fn relate(space: &xr::Space, base: &xr::Space, t: xr::Time) -> Motion {
    let Ok((loc, vel)) = space.relate(base, t) else {
        return Motion { pose: Pose::IDENTITY, linear_velocity: [0.0; 3], angular_velocity: [0.0; 3], flags: MotionFlags(0) };
    };
    let lf = loc.location_flags;
    let vf = vel.velocity_flags;
    let mut flags = 0;
    for (have, bit) in [
        (lf.contains(xr::SpaceLocationFlags::POSITION_VALID), MotionFlags::POSITION_VALID),
        (lf.contains(xr::SpaceLocationFlags::ORIENTATION_VALID), MotionFlags::ORIENTATION_VALID),
        (lf.contains(xr::SpaceLocationFlags::POSITION_TRACKED), MotionFlags::POSITION_TRACKED),
        (lf.contains(xr::SpaceLocationFlags::ORIENTATION_TRACKED), MotionFlags::ORIENTATION_TRACKED),
        (vf.contains(xr::SpaceVelocityFlags::LINEAR_VALID), MotionFlags::LINEAR_VELOCITY_VALID),
        (vf.contains(xr::SpaceVelocityFlags::ANGULAR_VALID), MotionFlags::ANGULAR_VELOCITY_VALID),
    ] {
        if have {
            flags |= bit;
        }
    }
    let (p, o) = (loc.pose.position, loc.pose.orientation);
    let (v, w) = (vel.linear_velocity, vel.angular_velocity);
    Motion {
        pose: Pose { position: [p.x, p.y, p.z], orientation: [o.x, o.y, o.z, o.w] },
        linear_velocity: [v.x, v.y, v.z],
        angular_velocity: [w.x, w.y, w.z],
        flags: MotionFlags(flags),
    }
}

impl TrackingSource for XrTracking {
    fn info(&self) -> HeadsetInfo {
        self.info.clone()
    }

    fn sample(&mut self, time_ns: i64) -> TrackingSample {
        let t = self.xr_time(time_ns);
        // Fails harmlessly (not focused) while the app is in the background.
        let _ = self.session.sync_actions(&[xr::ActiveActionSet::new(&self.actions.set)]);
        let horizon_ns = HEAD_HORIZON_US.load(Ordering::Relaxed) as i64 * 1000;
        // Orientation is predicted the whole way; the display's reprojection absorbs what is
        // left. Position only a little: extrapolated head translation turns every small
        // movement into swimming parallax, which reprojection cannot undo.
        let mut hmd = relate(&self.view, &self.stage, self.xr_time(time_ns + horizon_ns));
        let near = relate(&self.view, &self.stage, self.xr_time(time_ns + horizon_ns / 4));
        if near.flags.is_valid() {
            hmd.pose.position = near.pose.position;
        }
        if hmd.flags.is_valid() {
            self.last_hmd = Some(hmd);
        } else if let Some(last) = self.last_hmd {
            self.lost_hmd.0 += 1;
            let (count, since) = &mut self.lost_hmd;
            if since.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
                log::warn!("head not locatable for {count} samples; repeating the last good pose");
                (*count, *since) = (0, Some(std::time::Instant::now()));
            }
            hmd = Motion { linear_velocity: [0.0; 3], angular_velocity: [0.0; 3], ..last };
            hmd.flags.0 &= !(MotionFlags::LINEAR_VELOCITY_VALID | MotionFlags::ANGULAR_VELOCITY_VALID);
        }
        TrackingSample {
            sample_time_ns: time_ns,
            hmd,
            controllers: [self.controller(0, t), self.controller(1, t)],
        }
    }

    fn battery(&mut self) -> Option<Batteries> {
        let controllers =
            [0, 1].map(|hand| crate::battery::controller(&self.instance, &self.session, self.actions.hands[hand]));
        Some(Batteries { hmd: crate::battery::headset(&self.app), controllers })
    }

    fn haptics(&mut self, p: HapticPulse) {
        let vibration = xr::HapticVibration::new()
            .amplitude(p.amplitude.clamp(0.0, 1.0))
            .frequency(p.frequency_hz)
            .duration(xr::Duration::from_nanos(Duration::from_secs_f32(p.duration_s.max(0.0)).as_nanos() as i64));
        let _ = self.actions.haptic.apply_feedback(&self.session, self.actions.hands[p.hand as usize], &vibration);
    }
}
