//! Resolve the primaries the color LUT should encode for, from OS facts.
//!
//! Principle: out-of-box correctness is the app's job, and the OS supplies the
//! facts (it is not the user's job to configure a display profile). We ask the
//! OS what the panel is and whether the compositor is color-managing it, then:
//!   - managed, or an explicit sRGB/P3 target → encode for that space and let
//!     the compositor map our (untagged) output to the panel; or
//!   - a wide-gamut panel the compositor is NOT managing → compress into the
//!     panel's own measured primaries ourselves (a degradation, logged loudly
//!     like the interpreter/audio fallbacks).
//!
//! Today only Linux/Wayland detection is wired. macOS is always
//! compositor-managed (returns the configured target). Windows detection
//! (DXGI `GetDesc1` + DisplayConfig advanced-color state) slots in here the
//! same way and is not yet implemented.

use crate::color::Primaries;
use crate::profile::DisplayTarget;

/// The primaries the LUT should encode for, resolved from the user's
/// `configured` target plus OS detection. Only `Auto` auto-detects; explicit
/// `Srgb`/`DisplayP3` are honored verbatim.
pub fn resolve(configured: DisplayTarget) -> Primaries {
    #[cfg(target_os = "linux")]
    if configured == DisplayTarget::Auto {
        if let Some(panel) = resolve_auto_linux() {
            return panel;
        }
    }
    configured.primaries()
}

/// Linux/Wayland auto-detection. `Some(panel)` means "compress to these
/// measured panel primaries" (unmanaged wide-gamut fallback); `None` means
/// "nothing to do" (sRGB panel, or the compositor is managing — encode sRGB).
#[cfg(target_os = "linux")]
fn resolve_auto_linux() -> Option<Primaries> {
    use crate::color::{gamut_area, SRGB};

    // No wider-than-sRGB panel => nothing to correct toward.
    let panel = crate::edid::panel_primaries()?;
    let probe = crate::wayland_cm::probe_output_primaries();

    // Log what the OS reports — this is the signal the managed/unmanaged
    // decision rests on, so make it observable.
    match probe {
        Some(o) => {
            eprintln!(
            "color: wide-gamut panel (EDID gamut {:.3}); compositor reports output gamut {:.3} \
             [R {:.3},{:.3} G {:.3},{:.3} B {:.3},{:.3}]",
            gamut_area(&panel), gamut_area(&o),
            o.red.x, o.red.y, o.green.x, o.green.y, o.blue.x, o.blue.y,
        )
        }
        None => eprintln!(
            "color: wide-gamut panel (EDID gamut {:.3}); no compositor color-management protocol",
            gamut_area(&panel),
        ),
    }

    // The compositor is managing a wide output if it reports a gamut clearly
    // wider than sRGB. Validated 2026-06-14 on KWin (Plasma 6): in
    // sRGB-passthrough it reports its sRGB output *profile* (gamut 0.112), not
    // the panel's physical primaries (0.149) — so a sub-1.1x-sRGB report
    // reliably means "this wide panel is NOT being color-managed."
    //
    // Note this is informational, not a `DEGRADED:` line: the in-app
    // correction produces the *correct* result (it just does the compositor's
    // job inline), so the user experiences no downgrade. The genuinely
    // degraded case is the opposite one — a wide panel we *can't* read or
    // correct — which would oversaturate; that path returns None above.
    let managed = probe.is_some_and(|o| gamut_area(&o) > gamut_area(&SRGB) * 1.1);
    if managed {
        eprintln!("color: compositor color-manages this display; encoding sRGB");
        None
    } else {
        eprintln!(
            "color: compositor isn't color-managing this wide-gamut panel — \
             correcting in-app to its measured primaries \
             [R {:.3},{:.3} G {:.3},{:.3} B {:.3},{:.3}]",
            panel.red.x, panel.red.y, panel.green.x, panel.green.y, panel.blue.x, panel.blue.y,
        );
        Some(panel)
    }
}
