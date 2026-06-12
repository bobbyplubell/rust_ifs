// Full-GPU fractal flame: the chaos game itself runs here as a compute shader.
// Thousands of threads each iterate an independent trajectory and atomically
// accumulate into a histogram. Then a downsample+max pass and a tone-map pass
// (also compute) produce the final image. No CPU point generation.
//
// Genome is uploaded as a flat f32 buffer; each transform occupies STRIDE
// floats: [weight, color, affine(6), post(6), variations(22)] = 36.

const PI: f32 = 3.14159265358979;
const STRIDE: u32 = 36u;

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

fn variation(idx: u32, x: f32, y: f32, state: ptr<function, u32>) -> vec2<f32> {
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
    for (var v = 0u; v < 22u; v = v + 1u) {
        let w = field(t, 14u + v);
        if (w != 0.0) {
            b = b + w * variation(v, px, py, state);
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
        color = (color + field(t, 1u)) * 0.5;
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
            pc = (pc + field(n, 1u)) * 0.5;
            px = fp.x; py = fp.y;
        }

        let ix = params.cam.x * px + params.cam.y * py + params.cam.z;
        let iy = params.cam.w * px + params.cam2.x * py + params.cam2.y;
        if (ix >= 0.0 && iy >= 0.0 && ix < f32(hw) && iy < f32(hh)) {
            let pix = (u32(iy) * hw + u32(ix)) * 4u;
            let cidx = u32(clamp(pc, 0.0, 1.0) * 255.0) * 3u;
            atomicAdd(&hist[pix], 1u);
            atomicAdd(&hist[pix + 1u], u32(palette[cidx] * scale));
            atomicAdd(&hist[pix + 2u], u32(palette[cidx + 1u] * scale));
            atomicAdd(&hist[pix + 3u], u32(palette[cidx + 2u] * scale));
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

    let freq = me.x;
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
    let log_max = max(log(1.0 + f32(atomicLoad(&maxbuf[0]))), 1e-12);

    let avg = me.yzw / scale / freq;
    let l = clamp(log(1.0 + freq) / log_max * brightness, 0.0, 1.0);
    let lg = pow(l, inv_gamma);
    let by_brightness = avg * lg;
    let by_channel = pow(max(avg * l, vec3(0.0)), vec3(inv_gamma));
    var col = vib * by_brightness + (1.0 - vib) * by_channel;
    col = col + bg * (1.0 - l);
    textureStore(out_tex, vec2<i32>(i32(gid.x), i32(gid.y)), vec4(col, 1.0));
}
