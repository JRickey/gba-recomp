//! Probe the compositor's notion of the output's color volume (Linux/Wayland).
//!
//! Read-only and *isolated*: we open our OWN short-lived Wayland connection
//! (`connect_to_env`), never the one winit/wgpu render through. A Wayland
//! protocol error is fatal to its entire connection — so a color-management
//! hiccup here can, at worst, abort this probe and return `None`; it can never
//! poison the rendering connection. (An earlier version attached to winit's
//! display via `from_foreign_display` and a single rejected request took down
//! the whole app — never again.)
//!
//! We deliberately do NOT *tag* our surface. On Wayland the compositor already
//! treats untagged surfaces as sRGB, so a managed compositor maps our output
//! correctly without any declaration; tagging added only a fatal-error surface
//! for zero benefit. The macOS "declare sRGB" model does not port here.
//!
//! Mechanism: bind `wp_color_manager_v1` and a `wl_output`, ask for the
//! output's image description, read its `info` primaries. The caller compares
//! those against the panel's true (EDID) primaries to decide whether the
//! compositor is actually color-managing a wide-gamut panel.

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_output_v1::WpColorManagementOutputV1,
    wp_color_manager_v1::WpColorManagerV1,
    wp_image_description_info_v1::{self, WpImageDescriptionInfoV1},
    wp_image_description_v1::{self, WpImageDescriptionV1},
};

use crate::color::{Primaries, Xy};

/// Bounded `roundtrip` count for a multi-step exchange. Each roundtrip is
/// itself bounded (the server always answers the implicit sync), so this can
/// never hang even on a broken compositor.
const MAX_ROUNDTRIPS: u32 = 6;

/// Readiness + result state collected while we roundtrip our queue.
#[derive(Default)]
struct State {
    /// `Some(true)` once the output's image description is `ready`,
    /// `Some(false)` on `failed`.
    desc_ready: Option<bool>,
    /// xy primaries read from the description's `info`.
    info_primaries: Option<Primaries>,
}

/// What the compositor believes the (first) output's color primaries are, or
/// `None` if the protocol/output is unavailable (X11, no color management, or
/// any error on our isolated connection).
///
/// Per-window output matching is a multi-monitor TODO; the EDID side already
/// picks the widest-gamut connected panel.
pub fn probe_output_primaries() -> Option<Primaries> {
    // Our own connection — isolated from winit/wgpu's.
    let conn = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();

    let manager: WpColorManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
    let output: WlOutput = globals.bind(&qh, 1..=4, ()).ok()?;
    let cm_output: WpColorManagementOutputV1 = manager.get_output(&output, &qh, ());
    let desc = cm_output.get_image_description(&qh, ());

    let mut state = State::default();
    let mut guard = 0;
    while state.desc_ready.is_none() && guard < MAX_ROUNDTRIPS {
        queue.roundtrip(&mut state).ok()?;
        guard += 1;
    }
    if state.desc_ready != Some(true) {
        return None;
    }
    // `get_information` is only valid on a ready description.
    let _info = desc.get_information(&qh, ());
    let mut guard = 0;
    while state.info_primaries.is_none() && guard < MAX_ROUNDTRIPS {
        queue.roundtrip(&mut state).ok()?;
        guard += 1;
    }
    state.info_primaries
}

// --- Dispatch glue ---------------------------------------------------------

// registry_queue_init drives the registry internally; we only need the impl.
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpImageDescriptionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpImageDescriptionV1,
        event: <WpImageDescriptionV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_image_description_v1::Event::Ready { .. } => state.desc_ready = Some(true),
            wp_image_description_v1::Event::Failed { .. } => state.desc_ready = Some(false),
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpImageDescriptionInfoV1,
        event: <WpImageDescriptionInfoV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The compositor reports primaries as xy * 1_000_000.
        if let wp_image_description_info_v1::Event::Primaries {
            r_x,
            r_y,
            g_x,
            g_y,
            b_x,
            b_y,
            w_x,
            w_y,
        } = event
        {
            let xy = |x: i32, y: i32| Xy {
                x: x as f64 / 1_000_000.0,
                y: y as f64 / 1_000_000.0,
            };
            state.info_primaries = Some(Primaries {
                red: xy(r_x, r_y),
                green: xy(g_x, g_y),
                blue: xy(b_x, b_y),
                white: xy(w_x, w_y),
            });
        }
    }
}

// Proxies whose events we don't consume.
wayland_client::delegate_noop!(State: ignore WpColorManagerV1);
wayland_client::delegate_noop!(State: ignore WpColorManagementOutputV1);
wayland_client::delegate_noop!(State: ignore WlOutput);
