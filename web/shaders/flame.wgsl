// Full-GPU fractal flame: the chaos game itself runs here as a compute shader.
// Thousands of threads each iterate an independent trajectory and atomically
// accumulate into a histogram. Then a downsample+max pass and a tone-map pass
// (also compute) produce the final image. No CPU point generation.
//
// Genome is uploaded as a flat f32 buffer; each transform occupies STRIDE
// floats: [weight, color, affine(6), post(6), variations(49), pvals(28), color_speed] = 92.
// pvals layout mirrors flame-core variations::pval.

const PI: f32 = 3.14159265358979;
const STRIDE: u32 = 92u;

struct Params {
    dims: vec4<u32>,   // hw, hh, n_transforms, has_final
    ctrl: vec4<u32>,   // plot_iters, burn_in, seed, ss
    out_dims: vec4<u32>, // out_w, out_h, _, _
    cam: vec4<f32>,    // world->image affine a, b, c, d
    cam2: vec4<f32>,   // e, f, color_scale, total_weight
    tone: vec4<f32>,   // gamma, brightness, vibrancy, _
    bg: vec4<f32>,     // background rgb
};

@group(0) @binding(0) var<storage, read> genome: array<f32>;
@group(0) @binding(1) var<uniform> params: Params;
@group(0) @binding(2) var<storage, read_write> hist: array<atomic<u32>>;
@group(0) @binding(3) var<storage, read> palette: array<f32>; // 256 * 3
@group(0) @binding(4) var<storage, read_write> down: array<u32>; // out_w*out_h*4
@group(0) @binding(5) var<storage, read_write> maxbuf: array<atomic<u32>>; // [0]
@group(0) @binding(6) var out_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(7) var<storage, read_write> stats: array<atomic<u32>>; // [sum counts, nonzero cells]

// ---- PRNG (PCG, per-thread) ------------------------------------------------

fn pcg(state: ptr<function, u32>) -> u32 {
    let old = *state;
    *state = old * 747796405u + 2891336453u;
    let word = ((old >> ((old >> 28u) + 4u)) ^ old) * 277803737u;
    return (word >> 22u) ^ word;
}
fn rnd(state: ptr<function, u32>) -> f32 {
    return f32(pcg(state)) * (1.0 / 4294967296.0);
}

// ---- variations (mirror of flame-core variations.rs) -----------------------

fn pv(t: u32, k: u32) -> f32 {
    return genome[t * STRIDE + 63u + k]; // pvals after 49 variation weights
}
fn aff(t: u32, k: u32) -> f32 {
    return genome[t * STRIDE + 2u + k]; // pre-affine a..f for dependent variations
}

fn variation(idx: u32, t: u32, x: f32, y: f32, w: f32, state: ptr<function, u32>) -> vec2<f32> {
    let r2 = x * x + y * y;
    let r = sqrt(r2);
    let theta = atan2(x, y);
    let inv_r = select(0.0, 1.0 / r, r > 1e-12);
    switch idx {
        case 0u { return vec2(x, y); }
        case 1u { return vec2(sin(x), sin(y)); }
        case 2u { let s = 1.0 / (r2 + 1e-12); return vec2(s * x, s * y); }
        case 3u { let s = sin(r2); let c = cos(r2); return vec2(x * s - y * c, x * c + y * s); }
        case 4u { return vec2(inv_r * (x - y) * (x + y), inv_r * 2.0 * x * y); }
        case 5u { return vec2(theta / PI, r - 1.0); }
        case 6u { return vec2(r * sin(theta + r), r * cos(theta - r)); }
        case 7u { return vec2(r * sin(theta * r), -r * cos(theta * r)); }
        case 8u { let t = theta / PI; return vec2(t * sin(PI * r), t * cos(PI * r)); }
        case 9u { return vec2(inv_r * (cos(theta) + sin(r)), inv_r * (sin(theta) - cos(r))); }
        case 10u { return vec2(sin(theta) * inv_r, r * cos(theta)); }
        case 11u { return vec2(sin(theta) * cos(r), cos(theta) * sin(r)); }
        case 12u {
            let p0 = sin(theta + r); let p1 = cos(theta - r);
            let a = p0 * p0 * p0; let b = p1 * p1 * p1;
            return vec2(r * (a + b), r * (a - b));
        }
        case 13u {
            let sr = sqrt(r);
            let om = select(0.0, PI, rnd(state) < 0.5);
            let a = theta * 0.5 + om;
            return vec2(sr * cos(a), sr * sin(a));
        }
        case 14u { let e = exp(x - 1.0); return vec2(e * cos(PI * y), e * sin(PI * y)); }
        case 15u { let m = pow(r, sin(theta)); return vec2(m * cos(theta), m * sin(theta)); }
        case 16u { return vec2(cos(PI * x) * cosh(y), -sin(PI * x) * sinh(y)); }
        case 17u { let s = 2.0 / (r + 1.0); return vec2(s * x, s * y); }
        case 18u { let s = 4.0 / (r2 + 4.0); return vec2(s * x, s * y); }
        case 19u { return vec2(sin(x), y); }
        case 20u { return vec2(sin(x) / cos(y), tan(y)); }
        case 21u { let d = x * x - y * y; let s = sqrt(1.0 / (d * d + 1e-12)); return vec2(s * x, s * y); }
        // -- parametric (read the transform's pval block) --
        case 22u { // julian
            var power = pv(t, 0u); if (abs(power) < 1e-6) { power = 2.0; }
            let dist = pv(t, 1u);
            let k = trunc(abs(power) * rnd(state));
            let tt = (atan2(y, x) + 6.28318530717958648 * k) / power;
            let m = pow(r, dist / power);
            return vec2(m * cos(tt), m * sin(tt));
        }
        case 23u { // juliascope
            var power = pv(t, 2u); if (abs(power) < 1e-6) { power = 2.0; }
            let dist = pv(t, 3u);
            let k = trunc(abs(power) * rnd(state));
            let phi = atan2(y, x);
            let dir = select(phi, -phi, (u32(k) & 1u) == 1u);
            let tt = (6.28318530717958648 * k + dir) / power;
            let m = pow(r, dist / power);
            return vec2(m * cos(tt), m * sin(tt));
        }
        case 24u { // blob
            let low = pv(t, 4u); let high = pv(t, 5u); let waves = pv(t, 6u);
            let pr = r * (low + 0.5 * (high - low) * (sin(waves * theta) + 1.0));
            return vec2(pr * sin(theta), pr * cos(theta));
        }
        case 25u { // curl
            let c1 = pv(t, 7u); let c2 = pv(t, 8u);
            let re = 1.0 + c1 * x + c2 * (x * x - y * y);
            let im = c1 * y + 2.0 * c2 * x * y;
            let s = 1.0 / (re * re + im * im + 1e-12);
            return vec2((x * re + y * im) * s, (y * re - x * im) * s);
        }
        case 26u { // fan2
            let fx = pv(t, 9u); let fy = pv(t, 10u);
            let dx = PI * (fx * fx + 1e-10);
            let dx2 = 0.5 * dx;
            let tt = theta + fy - dx * trunc((theta + fy) / dx);
            let a = select(theta + dx2, theta - dx2, tt > dx2);
            return vec2(r * sin(a), r * cos(a));
        }
        case 27u { // rings2
            let v = pv(t, 11u);
            let p = v * v + 1e-10;
            let tt = r - 2.0 * p * trunc((r + p) / (2.0 * p)) + r * (1.0 - p);
            return vec2(tt * sin(theta), tt * cos(theta));
        }
        case 28u { // pdj
            return vec2(
                sin(pv(t, 12u) * y) - cos(pv(t, 13u) * x),
                sin(pv(t, 14u) * x) - cos(pv(t, 15u) * y));
        }
        case 29u { // bent
            var nx = x; var ny = y;
            if (x < 0.0) { nx = 2.0 * x; }
            if (y < 0.0) { ny = 0.5 * y; }
            return vec2(nx, ny);
        }
        case 30u { // waves (dependent)
            return vec2(
                x + aff(t, 1u) * sin(y / (aff(t, 2u) * aff(t, 2u) + 1e-10)),
                y + aff(t, 4u) * sin(x / (aff(t, 5u) * aff(t, 5u) + 1e-10)));
        }
        case 31u { // fisheye (reversed x/y)
            let s = 2.0 / (r + 1.0);
            return vec2(s * y, s * x);
        }
        case 32u { // popcorn (dependent)
            return vec2(
                x + aff(t, 2u) * sin(tan(3.0 * y)),
                y + aff(t, 5u) * sin(tan(3.0 * x)));
        }
        case 33u { // rings (dependent)
            let c2 = aff(t, 2u) * aff(t, 2u) + 1e-10;
            let m = (r + c2) - 2.0 * c2 * floor((r + c2) / (2.0 * c2));
            let tt = m - c2 + r * (1.0 - c2);
            return vec2(tt * cos(theta), tt * sin(theta));
        }
        case 34u { // fan (dependent)
            let tw = PI * (aff(t, 2u) * aff(t, 2u)) + 1e-10;
            let half = tw * 0.5;
            let q = theta + aff(t, 5u);
            let m = q - tw * floor(q / tw);
            var a = theta + half;
            if (m > half) { a = theta - half; }
            return vec2(r * cos(a), r * sin(a));
        }
        case 35u { // perspective
            let p1 = pv(t, 16u); let p2 = pv(t, 17u);
            let k = p2 / (p2 - y * sin(p1) + 1e-10);
            return vec2(k * x, k * y * cos(p1));
        }
        case 36u { // noise
            let p1 = rnd(state);
            let a = 6.28318530717958648 * rnd(state);
            return vec2(p1 * x * cos(a), p1 * y * sin(a));
        }
        case 37u { // blur
            let p1 = rnd(state);
            let a = 6.28318530717958648 * rnd(state);
            return vec2(p1 * cos(a), p1 * sin(a));
        }
        case 38u { // gaussian
            let s = rnd(state) + rnd(state) + rnd(state) + rnd(state) - 2.0;
            let a = 6.28318530717958648 * rnd(state);
            return vec2(s * cos(a), s * sin(a));
        }
        case 39u { // radial blur (weight-dependent)
            let p1 = pv(t, 18u) * (PI / 2.0);
            var v = w; if (abs(v) < 1e-9) { v = 1e-9; }
            let t1 = v * (rnd(state) + rnd(state) + rnd(state) + rnd(state) - 2.0);
            let phi = atan2(y, x);
            let t2 = phi + t1 * sin(p1);
            let t3 = t1 * cos(p1) - 1.0;
            return vec2((r * cos(t2) + t3 * x) / v, (r * sin(t2) + t3 * y) / v);
        }
        case 40u { // pie
            let p1 = max(pv(t, 19u), 1.0);
            let t1 = trunc(rnd(state) * p1 + 0.5);
            let t2 = pv(t, 20u) + (6.28318530717958648 / p1) * (t1 + rnd(state) * pv(t, 21u));
            let p = rnd(state);
            return vec2(p * cos(t2), p * sin(t2));
        }
        case 41u { // ngon
            let p1 = pv(t, 22u);
            let p2 = 6.28318530717958648 / max(pv(t, 23u), 1.0);
            let phi = atan2(y, x);
            let t3 = phi - p2 * floor(phi / p2);
            var t4 = t3;
            if (t3 <= p2 * 0.5) { t4 = t3 - p2; }
            let denom = max(pow(r, p1), 1e-10);
            let k = (pv(t, 24u) * (1.0 / (abs(cos(t4)) + 1e-10) - 1.0) + pv(t, 25u)) / denom;
            return vec2(k * x, k * y);
        }
        case 42u { // rectangles
            let p1 = pv(t, 26u); let p2 = pv(t, 27u);
            var nx = x; var ny = y;
            if (abs(p1) >= 1e-10) { nx = (2.0 * floor(x / p1) + 1.0) * p1 - x; }
            if (abs(p2) >= 1e-10) { ny = (2.0 * floor(y / p2) + 1.0) * p2 - y; }
            return vec2(nx, ny);
        }
        case 43u { // arch (weight-dependent)
            let a = rnd(state) * PI * w;
            let s = sin(a);
            return vec2(s, s * s / (cos(a) + 1e-10));
        }
        case 44u { return vec2(rnd(state) - 0.5, rnd(state) - 0.5); } // square
        case 45u { // rays (weight-dependent)
            var v = w; if (abs(v) < 1e-9) { v = 1e-9; }
            let k = v * tan(rnd(state) * PI * v) / (r2 + 1e-10);
            return vec2(k * cos(x), k * sin(y));
        }
        case 46u { // blade (weight-dependent)
            let a = rnd(state) * r * w;
            return vec2(x * (cos(a) + sin(a)), x * (cos(a) - sin(a)));
        }
        case 47u { // secant (weight-dependent)
            var v = w; if (abs(v) < 1e-9) { v = 1e-9; }
            return vec2(x, 1.0 / (v * cos(v * r) + 1e-10));
        }
        case 48u { // twintrian (weight-dependent)
            let a = rnd(state) * r * w;
            let s = sin(a);
            let tt = log(max(s * s, 1e-30)) / 2.30258509299404568 + cos(a);
            return vec2(x * tt, x * (tt - PI * s));
        }
        default { return vec2(x, y); }
    }
}

fn field(t: u32, off: u32) -> f32 {
    return genome[t * STRIDE + off];
}

fn apply_tx(t: u32, p: vec2<f32>, state: ptr<function, u32>) -> vec2<f32> {
    let px = field(t, 2u) * p.x + field(t, 3u) * p.y + field(t, 4u);
    let py = field(t, 5u) * p.x + field(t, 6u) * p.y + field(t, 7u);
    var b = vec2<f32>(0.0, 0.0);
    for (var v = 0u; v < 49u; v = v + 1u) {
        let w = field(t, 14u + v);
        if (w != 0.0) {
            b = b + w * variation(v, t, px, py, w, state);
        }
    }
    // post affine
    return vec2(
        field(t, 8u) * b.x + field(t, 9u) * b.y + field(t, 10u),
        field(t, 11u) * b.x + field(t, 12u) * b.y + field(t, 13u),
    );
}

fn pick(state: ptr<function, u32>) -> u32 {
    let n = params.dims.z;
    var rr = rnd(state) * params.cam2.w; // total_weight
    for (var i = 0u; i < n; i = i + 1u) {
        rr = rr - field(i, 0u);
        if (rr <= 0.0) { return i; }
    }
    return n - 1u;
}

// ---- pass 1: the chaos game ------------------------------------------------

@compute @workgroup_size(64)
fn cs_chaos(@builtin(global_invocation_id) gid: vec3<u32>) {
    let hw = params.dims.x;
    let hh = params.dims.y;
    let n = params.dims.z;
    let has_final = params.dims.w;
    let plot_iters = params.ctrl.x;
    let burn_in = params.ctrl.y;

    var state: u32 = params.ctrl.z ^ (gid.x * 2654435761u + 1u);
    var x = rnd(&state) * 2.0 - 1.0;
    var y = rnd(&state) * 2.0 - 1.0;
    var color = rnd(&state);

    let scale = params.cam2.z;
    let total = burn_in + plot_iters;
    for (var i = 0u; i < total; i = i + 1u) {
        let t = pick(&state);
        let np = apply_tx(t, vec2(x, y), &state);
        let cs = field(t, 91u);
        color = color * (1.0 - cs) + field(t, 1u) * cs;
        x = np.x;
        y = np.y;

        if (!(x == x) || !(y == y) || abs(x) > 1e6 || abs(y) > 1e6) {
            x = rnd(&state) * 2.0 - 1.0;
            y = rnd(&state) * 2.0 - 1.0;
            color = rnd(&state);
            continue;
        }
        if (i < burn_in) { continue; }

        var px = x; var py = y; var pc = color;
        if (has_final == 1u) {
            let fp = apply_tx(n, vec2(px, py), &state);
            let fcs = field(n, 91u);
            pc = pc * (1.0 - fcs) + field(n, 1u) * fcs;
            px = fp.x; py = fp.y;
        }

        let ix = params.cam.x * px + params.cam.y * py + params.cam.z;
        let iy = params.cam.w * px + params.cam2.x * py + params.cam2.y;
        if (ix >= 0.0 && iy >= 0.0 && ix < f32(hw) && iy < f32(hh)) {
            let pix = (u32(iy) * hw + u32(ix)) * 4u;
            let cidx = u32(clamp(pc, 0.0, 1.0) * 255.0) * 3u;
            // Count channel is 256-fixed-point so directional motion blur can
            // scale a step's contribution (tone.w = step intensity).
            let inten = params.tone.w;
            atomicAdd(&hist[pix], u32(256.0 * inten));
            atomicAdd(&hist[pix + 1u], u32(palette[cidx] * scale * inten));
            atomicAdd(&hist[pix + 2u], u32(palette[cidx + 1u] * scale * inten));
            atomicAdd(&hist[pix + 3u], u32(palette[cidx + 2u] * scale * inten));
        }
    }
}

// ---- pass 2: downsample ss x ss and track max density ----------------------

@compute @workgroup_size(64)
fn cs_downsample(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ow = params.out_dims.x;
    let oh = params.out_dims.y;
    if (gid.x >= ow * oh) { return; }
    let ox = gid.x % ow;
    let oy = gid.x / ow;
    let ss = params.ctrl.w;
    let hw = params.dims.x;

    var c = vec4<u32>(0u, 0u, 0u, 0u);
    for (var dy = 0u; dy < ss; dy = dy + 1u) {
        for (var dx = 0u; dx < ss; dx = dx + 1u) {
            let pix = ((oy * ss + dy) * hw + (ox * ss + dx)) * 4u;
            c.x = c.x + atomicLoad(&hist[pix]);
            c.y = c.y + atomicLoad(&hist[pix + 1u]);
            c.z = c.z + atomicLoad(&hist[pix + 2u]);
            c.w = c.w + atomicLoad(&hist[pix + 3u]);
        }
    }
    let base = gid.x * 4u;
    down[base] = c.x;
    down[base + 1u] = c.y;
    down[base + 2u] = c.z;
    down[base + 3u] = c.w;
    atomicMax(&maxbuf[0], c.x);
    // Density stats for the DE blur in cs_tonemap (sum fits u32: total
    // plotted points per preview frame << 2^32).
    atomicAdd(&stats[0], c.x);
    if (c.x > 0u) {
        atomicAdd(&stats[1], 1u);
    }
}

// ---- pass 3: tone map ------------------------------------------------------

// Density-estimation lite (mirror of the CPU tonemap in render.rs): sparse
// cells blend toward their 5x5 gaussian neighborhood so speckle melts into
// glow; dense cells stay sharp. Same precomputed kernel constants.
fn de_w(d2: u32) -> f32 {
    switch d2 {
        case 0u { return 1.0; }
        case 1u { return 0.6616; }
        case 2u { return 0.4376; }
        case 4u { return 0.1915; }
        case 5u { return 0.1267; }
        default { return 0.0367; } // 8
    }
}

// Cell as (count, r, g, b) in f32; count = -1 marks out of bounds.
fn de_cell(ix: i32, iy: i32, ow: i32, oh: i32) -> vec4<f32> {
    if (ix < 0 || iy < 0 || ix >= ow || iy >= oh) { return vec4(-1.0, 0.0, 0.0, 0.0); }
    let b = (u32(iy) * u32(ow) + u32(ix)) * 4u;
    return vec4(f32(down[b]), f32(down[b + 1u]), f32(down[b + 2u]), f32(down[b + 3u]));
}

@compute @workgroup_size(8, 8)
fn cs_tonemap(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ow = params.out_dims.x;
    let oh = params.out_dims.y;
    if (gid.x >= ow || gid.y >= oh) { return; }

    var me = de_cell(i32(gid.x), i32(gid.y), i32(ow), i32(oh));
    let nzc = atomicLoad(&stats[1]);
    if (nzc > 0u) {
        let mean_nz = f32(atomicLoad(&stats[0])) / f32(nzc);
        let t = clamp(me.x / max(mean_nz, 1e-12), 0.0, 1.0);
        if (t < 1.0) {
            var acc = vec4(0.0, 0.0, 0.0, 0.0);
            var ws = 0.0;
            for (var dy = -2; dy <= 2; dy = dy + 1) {
                for (var dx = -2; dx <= 2; dx = dx + 1) {
                    let c = de_cell(i32(gid.x) + dx, i32(gid.y) + dy, i32(ow), i32(oh));
                    if (c.x >= 0.0) {
                        let w = de_w(u32(dx * dx + dy * dy));
                        acc = acc + w * c;
                        ws = ws + w;
                    }
                }
            }
            me = t * me + (1.0 - t) * (acc / ws);
        }
    }

    let freq = me.x / 256.0; // count channel is 256-fixed-point
    let bg = params.bg.rgb;
    if (freq <= 0.0) {
        textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4(bg, 1.0));
        return;
    }

    let scale = params.cam2.z;
    let gamma = params.tone.x;
    let brightness = params.tone.y;
    let vib = params.tone.z;
    let inv_gamma = 1.0 / gamma;
    let log_max = max(log(1.0 + f32(atomicLoad(&maxbuf[0])) / 256.0), 1e-12);

    let avg = me.yzw / scale / freq / 256.0;
    let l = clamp(log(1.0 + freq) / log_max * brightness, 0.0, 1.0);
    let lg = pow(l, inv_gamma);
    let by_brightness = avg * lg;
    let by_channel = pow(max(avg * l, vec3(0.0)), vec3(inv_gamma));
    var col = vib * by_brightness + (1.0 - vib) * by_channel;
    col = col + bg * (1.0 - l);
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4(col, 1.0));
}
