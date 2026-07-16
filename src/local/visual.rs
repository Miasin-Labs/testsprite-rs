//! Visual regression: pixel-diff two PNG screenshots.
//!
//! A standalone comparison of two PNGs you point it at (the `visual` CLI
//! subcommand): a baseline and a current shot. It flags UI regressions with no
//! external service. Note: this is NOT auto-wired into `test run` and there is
//! no managed baseline store — you supply both images (e.g. a screenshot from a
//! `test run --browser <name>` and a saved reference); promoting a current shot
//! to the new baseline is a manual file copy.

use std::path::Path;

use image::GenericImageView;

/// Per-channel tolerance below which a pixel is considered unchanged (guards
/// against lossless-but-not-bit-identical PNG re-encoding noise).
const CHANNEL_TOLERANCE: u8 = 12;

/// A diff ratio above this fraction of changed pixels is a regression, unless
/// overridden. A fixed default is either too loose (false green on a real
/// regression) or too tight (flaky red on re-encoding noise) for every UI, so
/// [`regression_threshold`] lets a project tune it. Full data-driven
/// calibration needs a labeled corpus of good/bad shot pairs that this tool
/// does not manage, so the knob is an explicit override, not an inference.
pub const REGRESSION_THRESHOLD: f64 = 0.02;

/// The effective regression threshold: `TESTSPRITE_VISUAL_THRESHOLD` (a
/// fraction in `[0,1]`) when set and valid, else [`REGRESSION_THRESHOLD`].
pub fn regression_threshold() -> f64 {
    std::env::var("TESTSPRITE_VISUAL_THRESHOLD")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| (0.0..=1.0).contains(v))
        .unwrap_or(REGRESSION_THRESHOLD)
}

/// Result of comparing two screenshots.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisualDiff {
    /// Whether both images have the same width/height.
    pub same_dimensions: bool,
    /// Fraction of pixels that differ beyond tolerance, in `[0.0, 1.0]`.
    /// `1.0` when dimensions differ (nothing is comparable).
    pub diff_ratio: f64,
}

impl VisualDiff {
    /// Whether this diff crosses the effective [`regression_threshold`].
    pub fn is_regression(&self) -> bool {
        !self.same_dimensions || self.diff_ratio > regression_threshold()
    }
}

/// Compare two PNG screenshots pixel-by-pixel.
pub fn diff(baseline: &Path, current: &Path) -> anyhow::Result<VisualDiff> {
    let base = image::open(baseline)
        .map_err(|e| anyhow::anyhow!("could not open baseline {baseline:?}: {e}"))?;
    let curr = image::open(current)
        .map_err(|e| anyhow::anyhow!("could not open current {current:?}: {e}"))?;

    if base.dimensions() != curr.dimensions() {
        return Ok(VisualDiff {
            same_dimensions: false,
            diff_ratio: 1.0,
        });
    }

    let base = base.to_rgba8();
    let curr = curr.to_rgba8();
    let total = base.pixels().len();
    if total == 0 {
        return Ok(VisualDiff {
            same_dimensions: true,
            diff_ratio: 0.0,
        });
    }

    let differing = base
        .pixels()
        .zip(curr.pixels())
        .filter(|(a, b)| {
            a.0.iter()
                .zip(b.0.iter())
                .any(|(x, y)| x.abs_diff(*y) > CHANNEL_TOLERANCE)
        })
        .count();

    Ok(VisualDiff {
        same_dimensions: true,
        diff_ratio: differing as f64 / total as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_honors_a_valid_env_override_else_the_default() {
        // Default when unset.
        {
            let _g = crate::testutil::env_guard(&[("TESTSPRITE_VISUAL_THRESHOLD", None)]);
            assert_eq!(regression_threshold(), REGRESSION_THRESHOLD);
        }
        // A valid override is used, and flips is_regression at the new cutoff.
        {
            let _g = crate::testutil::env_guard(&[("TESTSPRITE_VISUAL_THRESHOLD", Some("0.10"))]);
            assert!((regression_threshold() - 0.10).abs() < 1e-9);
            let d = VisualDiff {
                same_dimensions: true,
                diff_ratio: 0.05,
            };
            // 5% diff is a regression at the 2% default but not at a 10% override.
            assert!(!d.is_regression());
        }
        // Out-of-range / garbage overrides fall back to the default.
        for bad in ["2.0", "-1", "nonsense", ""] {
            let _g = crate::testutil::env_guard(&[("TESTSPRITE_VISUAL_THRESHOLD", Some(bad))]);
            assert_eq!(regression_threshold(), REGRESSION_THRESHOLD, "{bad:?}");
        }
    }

    #[test]
    fn mismatched_dimensions_are_always_a_regression() {
        let d = VisualDiff {
            same_dimensions: false,
            diff_ratio: 1.0,
        };
        assert!(d.is_regression());
    }
}
