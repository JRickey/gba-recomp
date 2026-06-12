// Band-limited screen-grid + scaling pass.
//
// The grid algorithm: every subpixel stripe (3 vertical BGR stripes per
// source pixel) and every scanline row has a smooth polynomial aperture
// profile; for each output pixel we integrate that profile analytically
// over the output pixel's source-space footprint (the antiderivative is
// closed-form). All frequency content above the output Nyquist is removed
// exactly, so the grid cannot moire at any window size, integer scale or
// not, and energy is conserved as the window scales. This is a clean-room
// implementation of the published band-limited-grid integration math
// (aperture profiles (1-x^2-x^4+x^6)^2 horizontally, (1-2x^4+x^6)^2
// vertically).
//
// With grid_strength = 0 the pass degrades to exact area-averaged ("sharp
// pixel") scaling in display-encoded space: pixel-perfect at integer
// scales, alias-free at fractional ones.

struct Params {
    src_size: vec2f,     // source resolution (240, 160)
    dst_origin: vec2f,   // letterbox offset, physical px
    dst_size: vec2f,     // scaled image size, physical px
    grid_strength: f32,  // 0 = flat scaling, 1 = full grid
    gain: f32,           // pre-linearization gain inside the grid model
    gamma: f32,          // grid model linearization exponent (pure pow)
    tint: f32,           // stripe self-transmission (diagonal subpixel tint)
    stripe_d: f32,       // stripe aperture half-width, subpixel units
    row_d: f32,          // row aperture half-width, pixel units
    bgr: f32,            // 1.0 = blue stripe leftmost (panel order)
    srgb_surface: f32,   // 1.0 = surface view re-encodes sRGB; pre-decode output
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> P: Params;

struct VsOut {
    @builtin(position) pos: vec4f,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle.
    var out: VsOut;
    let x = f32(i32(vi & 1u) * 4 - 1);
    let y = f32(i32(vi >> 1u) * 4 - 1);
    out.pos = vec4f(x, y, 0.0, 1.0);
    return out;
}

fn fetch(x: i32, y: i32) -> vec3f {
    let sz = vec2i(P.src_size);
    let c = vec2i(clamp(x, 0, sz.x - 1), clamp(y, 0, sz.y - 1));
    return textureLoad(src, c, 0).rgb;
}

// Antiderivative of the squared horizontal stripe profile (1-x^2-x^4+x^6)^2.
fn smear_f_x(z: f32) -> f32 {
    let z2 = z * z;
    var zn = z;
    var ret = 0.0;
    // coefficients of the odd-power antiderivative terms
    var co = array<f32, 7>(1.0, -2.0 / 3.0, -0.2, 4.0 / 7.0, -1.0 / 9.0, -2.0 / 11.0, 1.0 / 13.0);
    for (var i = 0; i < 7; i++) {
        ret += zn * co[i];
        zn *= z2;
    }
    return ret;
}

// Antiderivative of the squared row profile (1-2x^4+x^6)^2.
fn smear_f_y(z: f32) -> f32 {
    let z2 = z * z;
    var zn = z;
    var ret = 0.0;
    var co = array<f32, 7>(1.0, 0.0, -4.0 / 5.0, 2.0 / 7.0, 4.0 / 9.0, -4.0 / 11.0, 1.0 / 13.0);
    for (var i = 0; i < 7; i++) {
        ret += zn * co[i];
        zn *= z2;
    }
    return ret;
}

// Integral of an aperture profile (half-width d, centered at offset x from
// the output pixel center) over a footprint of width dx, normalized by dx.
fn smear_x(x: f32, dx: f32, d: f32) -> f32 {
    let zl = clamp((x - dx * 0.5) / d, -1.0, 1.0);
    let zh = clamp((x + dx * 0.5) / d, -1.0, 1.0);
    return d * (smear_f_x(zh) - smear_f_x(zl)) / dx;
}

fn smear_y(x: f32, dx: f32, d: f32) -> f32 {
    let zl = clamp((x - dx * 0.5) / d, -1.0, 1.0);
    let zh = clamp((x + dx * 0.5) / d, -1.0, 1.0);
    return d * (smear_f_y(zh) - smear_f_y(zl)) / dx;
}

// 1-D overlap of the box footprint [p-du/2, p+du/2] with source cell
// [j, j+1], normalized by du: exact area-average ("sharp pixel") weight.
fn box_overlap(j: f32, p: f32, du: f32) -> f32 {
    let lo = max(j, p - du * 0.5);
    let hi = min(j + 1.0, p + du * 0.5);
    return max(hi - lo, 0.0) / du;
}

// Exact area-averaged scaling in encoded space.
fn flat_sample(p: vec2f, du: vec2f) -> vec3f {
    let ix = i32(floor(p.x));
    let iy = i32(floor(p.y));
    var acc = vec3f(0.0);
    for (var dy = -1; dy <= 1; dy++) {
        let wy = box_overlap(f32(iy + dy), p.y, du.y);
        if (wy <= 0.0) { continue; }
        for (var dx = -1; dx <= 1; dx++) {
            let wx = box_overlap(f32(ix + dx), p.x, du.x);
            if (wx <= 0.0) { continue; }
            acc += fetch(ix + dx, iy + dy) * wx * wy;
        }
    }
    return acc;
}

// Band-limited stripe/row grid, integrated in linear light.
fn grid_sample(p: vec2f, du: vec2f) -> vec3f {
    // Subpixel-space horizontal coordinates: 3 stripes per source pixel.
    let sx = p.x * 3.0;
    let dux = du.x * 3.0;
    let ix = i32(floor(p.x));
    // Rows are centered at integer+0.5; take the two rows bracketing p.y.
    let iy0 = i32(floor(p.y - 0.5));

    var acc = vec3f(0.0);
    for (var ry = 0; ry <= 1; ry++) {
        let jy = iy0 + ry;
        let wy = smear_y((f32(jy) + 0.5) - p.y, du.y, P.row_d);
        if (wy <= 0.0) { continue; }
        for (var rx = -1; rx <= 1; rx++) {
            let jx = ix + rx;
            let texel = fetch(jx, jy);
            // Linearize with the grid model's own response (pure power law,
            // matching the published parameterization).
            let lin = pow(max(texel * P.gain, vec3f(0.0)), vec3f(P.gamma));
            for (var ch = 0; ch < 3; ch++) {
                // Stripe index within the pixel for this channel; the
                // panel's stripe order is BGR (blue leftmost).
                var stripe = f32(ch);
                if (P.bgr > 0.5) { stripe = f32(2 - ch); }
                let center = f32(jx) * 3.0 + stripe + 0.5;
                let wx = smear_x(center - sx, dux, P.stripe_d);
                acc[ch] += lin[ch] * wx * wy;
            }
        }
    }
    return pow(max(acc * P.tint, vec3f(0.0)), vec3f(1.0 / P.gamma));
}

// Piecewise sRGB EOTF (decode): inverse of the encode an sRGB surface view
// applies on write. Our pipeline's output is already display-encoded bytes;
// when the only available surface format is *Srgb the hardware would encode
// them AGAIN, so we pre-decode the final color and the re-encode lands the
// intended bytes. Exact piecewise curve, not pow(2.2).
fn srgb_eotf(c: vec3f) -> vec3f {
    let lo = c / 12.92;
    let hi = pow((c + vec3f(0.055)) / 1.055, vec3f(2.4));
    return select(hi, lo, c <= vec3f(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let q = in.pos.xy - P.dst_origin;
    var rgb = vec3f(0.0); // letterbox black (fixed point of the EOTF)
    if (q.x >= 0.0 && q.y >= 0.0 && q.x < P.dst_size.x && q.y < P.dst_size.y) {
        let scale = P.dst_size / P.src_size;
        let p = q / scale;          // source-space position, pixel units
        let du = 1.0 / scale;       // source pixels per output pixel

        let flat = flat_sample(p, du);
        // The grid needs headroom to resolve: below ~2x it is sub-Nyquist
        // physically and we fade it out rather than render a wrong residue.
        let strength = P.grid_strength * smoothstep(1.5, 2.5, min(scale.x, scale.y));
        if (strength <= 0.0) {
            rgb = flat;
        } else {
            rgb = mix(flat, grid_sample(p, du), strength);
        }
    }
    if (P.srgb_surface > 0.5) {
        rgb = srgb_eotf(clamp(rgb, vec3f(0.0), vec3f(1.0)));
    }
    return vec4f(rgb, 1.0);
}
