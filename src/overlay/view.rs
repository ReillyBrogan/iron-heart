use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayView {
    pub text: String,
    pub classes: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub struct ViewParams {
    pub stale_after: Duration,
    /// 0 disables the lower limit
    pub low_bpm: u16,
    /// 0 disables the upper limit
    pub high_bpm: u16,
}

/// Decides what the overlay should show.
///
/// `since_last_packet` is `None` when no valid reading has arrived (or after a disconnect).
pub fn format_state(
    last_bpm: u16,
    since_last_packet: Option<Duration>,
    params: &ViewParams,
) -> OverlayView {
    let Some(age) = since_last_packet.filter(|_| last_bpm > 0) else {
        return OverlayView {
            text: "♥ --".to_owned(),
            classes: vec!["disconnected"],
        };
    };

    let mut classes = vec!["connected"];
    if age > params.stale_after {
        classes.push("stale");
    }
    let too_low = params.low_bpm > 0 && last_bpm < params.low_bpm;
    let too_high = params.high_bpm > 0 && last_bpm > params.high_bpm;
    if too_low || too_high {
        classes.push("alarm");
    }

    OverlayView {
        text: format!("♥ {last_bpm}"),
        classes,
    }
}

const BORDER_START_PX: f64 = 24.0;
const BORDER_FULL_PX: f64 = 220.0;
const BORDER_GROW_TIME: Duration = Duration::from_secs(90);
const BORDER_FLASH_PERIOD_SECS: f64 = 1.0;
const BORDER_MIN_ALPHA: f64 = 0.25;
const BORDER_MAX_ALPHA: f64 = 0.85;

/// How the alarm border should look at a given moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderFrame {
    /// Opacity multiplier, 0.0 to 1.0
    pub alpha: f64,
    /// Width of the gradient in logical pixels
    pub thickness: f64,
}

/// `elapsed` is the time since the heart rate first went out of range.
///
/// The border pulses at full strength first, then slowly widens (fast at first, easing off)
/// until it reaches its full size.
pub fn border_frame(elapsed: Duration) -> BorderFrame {
    let secs = elapsed.as_secs_f64();
    let pulse = 0.5 + 0.5 * (std::f64::consts::TAU * secs / BORDER_FLASH_PERIOD_SECS).cos();
    let alpha = BORDER_MIN_ALPHA + (BORDER_MAX_ALPHA - BORDER_MIN_ALPHA) * pulse;

    let progress = (secs / BORDER_GROW_TIME.as_secs_f64()).min(1.0);
    let eased = 1.0 - (1.0 - progress).powi(2);
    let thickness = BORDER_START_PX + (BORDER_FULL_PX - BORDER_START_PX) * eased;

    BorderFrame { alpha, thickness }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALE: Duration = Duration::from_secs(5);

    fn params(low_bpm: u16, high_bpm: u16) -> ViewParams {
        ViewParams {
            stale_after: STALE,
            low_bpm,
            high_bpm,
        }
    }

    fn secs(s: u64) -> Option<Duration> {
        Some(Duration::from_secs(s))
    }

    #[test]
    fn disconnected_without_packet() {
        let view = format_state(72, None, &params(0, 0));
        assert_eq!(view.text, "♥ --");
        assert_eq!(view.classes, vec!["disconnected"]);
    }

    #[test]
    fn disconnected_with_zero_bpm() {
        let view = format_state(0, secs(0), &params(0, 0));
        assert_eq!(view.text, "♥ --");
        assert_eq!(view.classes, vec!["disconnected"]);
    }

    #[test]
    fn connected() {
        let view = format_state(72, secs(1), &params(0, 0));
        assert_eq!(view.text, "♥ 72");
        assert_eq!(view.classes, vec!["connected"]);
    }

    #[test]
    fn stale_boundary() {
        let at_limit = format_state(72, Some(STALE), &params(0, 0));
        assert_eq!(at_limit.classes, vec!["connected"]);
        let past_limit = format_state(72, Some(STALE + Duration::from_millis(1)), &params(0, 0));
        assert_eq!(past_limit.classes, vec!["connected", "stale"]);
    }

    #[test]
    fn alarm_below_low() {
        let view = format_state(59, secs(1), &params(60, 100));
        assert_eq!(view.classes, vec!["connected", "alarm"]);
    }

    #[test]
    fn alarm_above_high() {
        let view = format_state(101, secs(1), &params(60, 100));
        assert_eq!(view.classes, vec!["connected", "alarm"]);
    }

    #[test]
    fn no_alarm_at_limits() {
        assert_eq!(
            format_state(60, secs(1), &params(60, 100)).classes,
            vec!["connected"]
        );
        assert_eq!(
            format_state(100, secs(1), &params(60, 100)).classes,
            vec!["connected"]
        );
    }

    #[test]
    fn limits_disabled_when_zero() {
        // No lower limit
        assert_eq!(
            format_state(30, secs(1), &params(0, 100)).classes,
            vec!["connected"]
        );
        // No upper limit
        assert_eq!(
            format_state(250, secs(1), &params(60, 0)).classes,
            vec!["connected"]
        );
    }

    #[test]
    fn alarm_persists_when_stale() {
        let view = format_state(150, Some(STALE * 2), &params(60, 100));
        assert_eq!(view.classes, vec!["connected", "stale", "alarm"]);
    }

    #[test]
    fn no_alarm_when_disconnected() {
        let view = format_state(0, None, &params(60, 100));
        assert_eq!(view.classes, vec!["disconnected"]);
    }

    #[test]
    fn border_starts_small_and_bright() {
        let frame = border_frame(Duration::ZERO);
        assert_eq!(frame.thickness, BORDER_START_PX);
        assert!((frame.alpha - BORDER_MAX_ALPHA).abs() < 1e-9);
    }

    #[test]
    fn border_flashes() {
        let half = border_frame(Duration::from_secs_f64(BORDER_FLASH_PERIOD_SECS / 2.0));
        assert!((half.alpha - BORDER_MIN_ALPHA).abs() < 1e-9);
        let full = border_frame(Duration::from_secs_f64(BORDER_FLASH_PERIOD_SECS));
        assert!((full.alpha - BORDER_MAX_ALPHA).abs() < 1e-9);
    }

    #[test]
    fn border_alpha_stays_in_range() {
        for ms in (0..200_000).step_by(37) {
            let alpha = border_frame(Duration::from_millis(ms)).alpha;
            assert!((BORDER_MIN_ALPHA..=BORDER_MAX_ALPHA).contains(&alpha));
        }
    }

    #[test]
    fn border_grows_then_stops() {
        let mut previous = 0.0;
        for secs in 0..=90 {
            let thickness = border_frame(Duration::from_secs(secs)).thickness;
            assert!(thickness >= previous);
            previous = thickness;
        }
        assert_eq!(previous, BORDER_FULL_PX);
        assert_eq!(
            border_frame(BORDER_GROW_TIME * 10).thickness,
            BORDER_FULL_PX
        );
    }
}
