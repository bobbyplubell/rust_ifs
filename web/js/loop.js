// loop.js — a render-decoupled animation player.
//
// A single requestAnimationFrame ticker draws an N-frame loop onto a canvas,
// cross-fading adjacent frames so playback is smooth even at a low frame count
// (64 frames over 14 s is ~4.6 fps — pure stepping without the cross-fade) and
// regardless of when, or whether, each frame is ready.
//
// The player NEVER renders. It only draws whatever `getFrame(i)` returns *right
// now* — an ImageBitmap / canvas, or null if that frame isn't ready yet. So the
// renderer and the display are fully decoupled: frames can pop in, upgrade in
// quality, or be missing, and the loop keeps playing smoothly. rAF also
// auto-throttles when the tab is backgrounded.
export class FrameLoop {
  constructor(canvas, { nFrames, loopMs = 14_000, getFrame }) {
    this.cv = canvas;
    this.ctx = canvas.getContext('2d');
    this.n = nFrames;
    this.loopMs = loopMs;
    this.getFrame = getFrame;
    this.running = false;
    // Boomerang range: while the swarm is still filling frames in, playback
    // ping-pongs over the CONTIGUOUS span [lo..hi] of frames that actually have
    // rendered content, so it's always moving instead of stalling on gaps. Set
    // to null (the default) for the normal full forward 0..n loop. setRange()
    // updates this live as more frames become ready, and clearing it (null)
    // resumes the full loop once everything's in.
    this.range = null;
    this._raf = (t) => this._tick(t);
  }

  // Drive boomerang playback over a contiguous ready span. Pass null/undefined
  // (or the full 0..n-1 span) to resume the normal forward loop.
  setRange(lo, hi) {
    if (lo == null || hi == null || hi <= lo || (lo === 0 && hi >= this.n - 1)) {
      this.range = null;
    } else {
      this.range = { lo, hi };
    }
  }

  start() {
    if (!this.running) { this.running = true; requestAnimationFrame(this._raf); }
  }

  stop() { this.running = false; }

  // The (fractional) frame position to draw at time `now`. Normally a forward
  // sweep over 0..n; in boomerang mode a triangle wave over [lo..hi] so the
  // span is traversed forward then reverse with no jump-cut at the ends.
  _position(now) {
    const t = (now % this.loopMs) / this.loopMs; // 0..1 phase of the loop
    if (this.range) {
      const { lo, hi } = this.range;
      const span = hi - lo;                       // # of steps one-way
      const tri = 1 - Math.abs(1 - 2 * t);        // 0→1→0 triangle wave
      return lo + tri * span;
    }
    return t * this.n;
  }

  _tick(now) {
    if (!this.running) return;
    const { cv, ctx, n } = this;
    if (cv.width && cv.height) {
      const pos = this._position(now);
      // Clamp the upper neighbour to the active span so the cross-fade never
      // reaches past the ready frames (or wraps) while boomeranging.
      const hi = this.range ? this.range.hi : n - 1;
      const i = Math.min(Math.floor(pos), hi);
      const frac = pos - Math.floor(pos);
      const next = this.range ? Math.min(i + 1, hi) : (i + 1) % n;
      const a = this.getFrame(i) || null;
      const b = this.getFrame(next) || a; // hold on a if the next isn't ready
      if (a) {
        ctx.globalAlpha = 1;
        ctx.drawImage(a, 0, 0, cv.width, cv.height);
        if (b && b !== a) {
          ctx.globalAlpha = frac;      // cross-fade into the next keyframe
          ctx.drawImage(b, 0, 0, cv.width, cv.height);
          ctx.globalAlpha = 1;
        }
      }
    }
    requestAnimationFrame(this._raf);
  }
}
