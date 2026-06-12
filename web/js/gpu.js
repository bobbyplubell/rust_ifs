// gpu.js — WebGPU preview renderer driving shaders/flame.wgsl (the full-GPU
// chaos game ported from the old native wgpu renderer, see attic/).
//
// PREVIEW ONLY, by design: the GPU runs per-thread PCG RNG, so its pixel
// stream is a different (equally valid) sample of the same attractor than the
// deterministic CPU protocol render — it can never produce proofs, and its
// tonemap is the older max-normalized one. What it buys is raw speed: full
//-screen animation at frame rates the CPU path cannot approach.

const STRIDE = 36; // floats per transform: [weight, color, affine6, post6, var22]
const COLOR_SCALE = 256.0;
const BURN_IN = 20;
const THREADS = 1 << 16; // parallel trajectories per frame (preview-sized)

const TAU = Math.PI * 2;

// Mirror of flame-core animate.rs (display-only, so float identity with the
// Rust version is not required): rotate each transform's affine basis through
// 2π per loop, drift the palette coordinate once around.
function animated(genome, phase) {
  const th = phase * TAU;
  const cos = Math.cos(th);
  const sin = Math.sin(th);
  const g = structuredClone(genome);
  for (const t of g.transforms) {
    const { a, b, d, e } = t.affine;
    t.affine.a = a * cos + b * sin;
    t.affine.b = -a * sin + b * cos;
    t.affine.d = d * cos + e * sin;
    t.affine.e = -d * sin + e * cos;
    t.color = (((t.color + phase) % 1) + 1) % 1;
  }
  if (g.final_transform) {
    const f = g.final_transform;
    f.color = (((f.color + phase) % 1) + 1) % 1;
  }
  return g;
}

function packTransform(out, base, t) {
  out[base] = t.weight;
  out[base + 1] = t.color;
  const A = t.affine, P = t.post;
  out.set([A.a, A.b, A.c, A.d, A.e, A.f, P.a, P.b, P.c, P.d, P.e, P.f], base + 2);
  for (let v = 0; v < 22; v++) out[base + 14 + v] = t.variations[v] || 0;
}

function packGenome(g) {
  const n = g.transforms.length;
  const hasFinal = g.final_transform ? 1 : 0;
  const out = new Float32Array((n + hasFinal) * STRIDE);
  g.transforms.forEach((t, i) => packTransform(out, i * STRIDE, t));
  if (hasFinal) packTransform(out, n * STRIDE, g.final_transform);
  return { data: out, n, hasFinal };
}

// 256-entry palette LUT (mirror of Palette::color stop interpolation).
function packPalette(palette) {
  const stops = palette.stops;
  const out = new Float32Array(256 * 3);
  for (let i = 0; i < 256; i++) {
    const c = i / 255;
    let lo = stops[0];
    let hi = stops[stops.length - 1];
    for (let s = 0; s + 1 < stops.length; s++) {
      if (c >= stops[s].pos && c <= stops[s + 1].pos) { lo = stops[s]; hi = stops[s + 1]; break; }
    }
    const span = Math.max(hi.pos - lo.pos, 1e-12);
    const t = Math.min(Math.max((c - lo.pos) / span, 0), 1);
    for (let k = 0; k < 3; k++) out[i * 3 + k] = lo.rgb[k] + (hi.rgb[k] - lo.rgb[k]) * t;
  }
  return out;
}

export class GpuFlame {
  /** Resolves to a GpuFlame or null when WebGPU is unavailable. */
  static async create() {
    try {
      if (!navigator.gpu) return null;
      const adapter = await navigator.gpu.requestAdapter();
      if (!adapter) return null;
      const device = await adapter.requestDevice();
      const wgsl = await (await fetch(new URL('../shaders/flame.wgsl', import.meta.url))).text();
      const flame = new GpuFlame(device, device.createShaderModule({ code: wgsl }));
      flame.adapterInfo = adapter.info ?? {};
      return flame;
    } catch (err) {
      console.warn('WebGPU unavailable:', err);
      return null;
    }
  }

  constructor(device, module) {
    this.device = device;
    const layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
        { binding: 1, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'uniform' } },
        { binding: 2, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
        { binding: 3, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
        { binding: 4, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
        { binding: 5, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
        { binding: 6, visibility: GPUShaderStage.COMPUTE,
          storageTexture: { access: 'write-only', format: 'rgba8unorm' } },
        { binding: 7, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
      ],
    });
    this.layout = layout;
    const pl = device.createPipelineLayout({ bindGroupLayouts: [layout] });
    this.pipelines = Object.fromEntries(['cs_chaos', 'cs_downsample', 'cs_tonemap'].map((e) => [
      e, device.createComputePipeline({ layout: pl, compute: { module, entryPoint: e } }),
    ]));
    this.dims = null; // current buffer geometry
  }

  /** (Re)attach a canvas for presentation. */
  configure(canvas) {
    this.ctx = canvas.getContext('webgpu');
    this.ctx.configure({
      device: this.device,
      format: 'rgba8unorm',
      usage: GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT,
      alphaMode: 'opaque',
    });
  }

  _ensure(w, h, ss, genomeFloats, steps) {
    const key = `${w}x${h}x${ss}x${genomeFloats}x${steps}`;
    if (this.dims === key) return;
    this.dims = key;
    const d = this.device;
    const hw = w * ss, hh = h * ss;
    this.buf = {
      // Per temporal step: its own genome + params (sub-phase, seed); the
      // histogram and everything downstream are shared (that's the blur).
      genome: Array.from({ length: steps }, () =>
        d.createBuffer({ size: genomeFloats * 4, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST })),
      params: Array.from({ length: steps }, () =>
        d.createBuffer({ size: 112, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST })),
      hist: d.createBuffer({ size: hw * hh * 16, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST }),
      palette: d.createBuffer({ size: 768 * 4, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST }),
      down: d.createBuffer({ size: w * h * 16, usage: GPUBufferUsage.STORAGE }),
      max: d.createBuffer({ size: 4, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST }),
      stats: d.createBuffer({ size: 8, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST }),
    };
    this.tex = d.createTexture({
      size: { width: w, height: h },
      format: 'rgba8unorm',
      usage: GPUTextureUsage.STORAGE_BINDING | GPUTextureUsage.COPY_SRC,
    });
    this.bindGroups = Array.from({ length: steps }, (_, k) => d.createBindGroup({
      layout: this.layout,
      entries: [
        { binding: 0, resource: { buffer: this.buf.genome[k] } },
        { binding: 1, resource: { buffer: this.buf.params[k] } },
        { binding: 2, resource: { buffer: this.buf.hist } },
        { binding: 3, resource: { buffer: this.buf.palette } },
        { binding: 4, resource: { buffer: this.buf.down } },
        { binding: 5, resource: { buffer: this.buf.max } },
        { binding: 6, resource: this.tex.createView() },
        { binding: 7, resource: { buffer: this.buf.stats } },
      ],
    }));
  }

  /** Render one frame of `genomeJson` at loop `phase` into the configured
   *  canvas. Resolves when the GPU work is submitted and done.
   *
   *  `keepExposure`: don't reset the running max-density used for tone-map
   *  normalization. Per-frame normalization makes a spinning flame's global
   *  brightness pump/flicker (the max moves as the attractor rotates); keeping
   *  the running max across a spin stabilizes exposure within one loop. */
  async frame(genomeJson, phase, {
    width, height, ss = 1, samples = 4_000_000, seed = 7, keepExposure = false,
    shutter = 0, temporal = 1,
  } = {}) {
    const base = typeof genomeJson === 'string' ? JSON.parse(genomeJson) : genomeJson;
    const steps = shutter > 0 ? Math.max(1, temporal) : 1;

    // flam3-style temporal samples: split the budget across `steps` sub-phases
    // spanning `shutter` (loop-phase units), all into ONE histogram — motion
    // blur in linear space ahead of the log tone map. Cost-neutral.
    const subs = Array.from({ length: steps }, (_, k) =>
      animated(base, phase + (steps > 1 ? shutter * (k / steps) : 0)));
    const packed = subs.map(packGenome);
    this._ensure(width, height, ss, packed[0].data.length, steps);

    const d = this.device;
    d.queue.writeBuffer(this.buf.palette, 0, packPalette(subs[0].palette));

    // Camera world->image affine for the supersampled grid (Camera::world_to_image).
    const hw = width * ss, hh = height * ss;
    const cam = subs[0].camera;
    const s = cam.scale * Math.min(hw, hh);
    const cos = Math.cos(cam.rotate), sin = Math.sin(cam.rotate);
    const a = s * cos, b = -s * sin, c = hw * 0.5 - s * (cos * cam.center_x - sin * cam.center_y);
    const dd = s * sin, e = s * cos, f = hh * 0.5 - s * (sin * cam.center_x + cos * cam.center_y);
    const totalWeight = subs[0].transforms.reduce((acc, t) => acc + t.weight, 0);
    const plotIters = Math.max(1, Math.ceil(samples / steps / THREADS));

    for (let k = 0; k < steps; k++) {
      d.queue.writeBuffer(this.buf.genome[k], 0, packed[k].data);
      const params = new ArrayBuffer(112);
      new Uint32Array(params, 0, 12).set([
        hw, hh, packed[k].n, packed[k].hasFinal,
        plotIters, BURN_IN, (seed + k * 0x9e3779b9) >>> 0, ss,
        width, height, 0, 0,
      ]);
      new Float32Array(params, 48, 16).set([
        a, b, c, dd,
        e, f, COLOR_SCALE, totalWeight,
        subs[0].gamma, subs[0].brightness, Math.min(Math.max(subs[0].vibrancy, 0), 1), 0,
        subs[0].background[0], subs[0].background[1], subs[0].background[2], 0,
      ]);
      d.queue.writeBuffer(this.buf.params[k], 0, params);
    }

    const enc = d.createCommandEncoder();
    enc.clearBuffer(this.buf.hist);
    enc.clearBuffer(this.buf.stats); // density stats are per-frame
    if (!keepExposure) enc.clearBuffer(this.buf.max); // exposure may be sticky
    const pass = enc.beginComputePass();
    pass.setPipeline(this.pipelines.cs_chaos);
    for (let k = 0; k < steps; k++) {
      pass.setBindGroup(0, this.bindGroups[k]);
      pass.dispatchWorkgroups(THREADS / 64);
    }
    pass.setBindGroup(0, this.bindGroups[0]);
    pass.setPipeline(this.pipelines.cs_downsample);
    pass.dispatchWorkgroups(Math.ceil((width * height) / 64));
    pass.setPipeline(this.pipelines.cs_tonemap);
    pass.dispatchWorkgroups(Math.ceil(width / 8), Math.ceil(height / 8));
    pass.end();
    enc.copyTextureToTexture(
      { texture: this.tex }, { texture: this.ctx.getCurrentTexture() },
      { width, height },
    );
    d.queue.submit([enc.finish()]);
    // Some implementations (notably software adapters) reject this promise
    // with instance-lifetime errors even though the work completes — fall
    // back to a small delay rather than failing the frame.
    await d.queue.onSubmittedWorkDone().catch(() => new Promise((r) => setTimeout(r, 50)));
  }
}
