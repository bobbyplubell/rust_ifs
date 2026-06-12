// Two passes:
//   1. points  — additively blend each plotted point's color into an HDR
//                 accumulation texture (rgb = summed color, a = density count).
//   2. tonemap — fullscreen pass turning the accumulation into the final image
//                 via Draves log-density + gamma + vibrancy (same math as the
//                 CPU renderer in flame-core), with ss-box downsampling.

// ---- pass 1: additive points ----------------------------------------------

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
};

@vertex
fn vs_points(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> VOut {
    var o: VOut;
    o.pos = vec4<f32>(p, 0.0, 1.0);
    o.color = c;
    return o;
}

@fragment
fn fs_points(in: VOut) -> @location(0) vec4<f32> {
    // Additive blend state sums these into the accumulation target.
    return vec4<f32>(in.color, 1.0);
}

// ---- pass 2: tone map ------------------------------------------------------

struct Tone {
    dims: vec4<u32>,    // out_w, out_h, ss, _pad
    params: vec4<f32>,  // gamma, brightness, vibrancy, log_max
    bg: vec4<f32>,      // background rgb in .xyz
};

@group(0) @binding(0) var accum_tex: texture_2d<f32>;
@group(0) @binding(1) var<uniform> tone: Tone;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // One oversized triangle covering the viewport.
    var pts = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(pts[vi], 0.0, 1.0);
}

@fragment
fn fs_tonemap(@builtin(position) fragpos: vec4<f32>) -> @location(0) vec4<f32> {
    let ox = u32(fragpos.x);
    let oy = u32(fragpos.y);
    let ss = tone.dims.z;

    // Box-downsample the ss x ss accumulation block for this output pixel.
    var acc = vec4<f32>(0.0);
    for (var dy: u32 = 0u; dy < ss; dy = dy + 1u) {
        for (var dx: u32 = 0u; dx < ss; dx = dx + 1u) {
            let sx = i32(ox * ss + dx);
            let sy = i32(oy * ss + dy);
            acc = acc + textureLoad(accum_tex, vec2<i32>(sx, sy), 0);
        }
    }

    let bg = tone.bg.rgb;
    let freq = acc.a;
    if (freq <= 0.0) {
        return vec4<f32>(bg, 1.0);
    }

    let gamma = tone.params.x;
    let brightness = tone.params.y;
    let vib = tone.params.z;
    let log_max = tone.params.w;
    let inv_gamma = 1.0 / gamma;

    let avg = acc.rgb / freq;
    let l = clamp(log(1.0 + freq) / log_max * brightness, 0.0, 1.0);
    let lg = pow(l, inv_gamma);
    let by_brightness = avg * lg;
    let by_channel = pow(max(avg * l, vec3<f32>(0.0)), vec3<f32>(inv_gamma));
    var col = vib * by_brightness + (1.0 - vib) * by_channel;
    col = col + bg * (1.0 - l);
    return vec4<f32>(col, 1.0);
}
