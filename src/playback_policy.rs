//! Compatibility facade for pure playback policy now owned by the internal core crate.
//!
//! The App and daemon intentionally keep this path stable while their shared, side-effect-free
//! rules live below both owners in `yututui-core`.

pub use yututui_core::playback::*;

#[cfg(test)]
mod tests {
    use super::PlaybackModeState;
    use crate::queue::Repeat;

    #[test]
    fn root_reexports_preserve_repeat_identity_and_serde_wire_format() {
        fn accepts_core(_: yututui_core::Repeat) {}

        let repeat = Repeat::One;
        accepts_core(repeat);
        let state = PlaybackModeState::new(repeat, false);
        let _: yututui_core::playback::PlaybackModeState = state;
        assert_eq!(serde_json::to_string(&repeat).unwrap(), "\"one\"");
        assert_eq!(
            serde_json::from_str::<Repeat>("\"all\"").unwrap(),
            Repeat::All
        );
    }
}
