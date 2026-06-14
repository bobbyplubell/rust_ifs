// loop.js — a render-decoupled animation player.
//
// A single requestAnimationFrame ticker draws an N-frame loop onto a canvas,
// cross-fading adjacent frames so playback is smooth even at a low frame count
// (128 frames over 14 s is ~9 fps — pure stepping without the cross-fade) and
// regardless of when, or whether, each frame is ready.
//
// The player NEVER renders. It only draws whatever `getFrame(i)` returns *right
// now* — an ImageBitmap / canvas, or null if that frame isn't ready yet. So the
// renderer and the display are fully decoupled: the FULL forward loop always
// plays, and each frame can pop in or UPGRADE IN PLACE (a fuzzy low-res preview
// → a sharp full-res render) without ever stalling or restricting playback. rAF
// also auto-throttles when the tab is backgrounded.
export class FrameLoop {
  constructor(canvas, { nFrames, loopMs = 14_000, getFrame }) {
    this.cv = canvas;
    this.ctx = canvas.getContext('2d');
    this.n = nFrames;
    this.loopMs = loopMs;
    this.getFrame = getFrame;
    this.running = false;
    this._raf = (t) => this._tick(t);
  }

  // The integer frame index currently on screen, for callers that want to
  // prioritize work near the playhead. Pure read of the same clock the ticker
  // draws from — no side effects.
  currentFrame(now = performance.now()) {
    return Math.min(Math.floor(this._position(now)), this.n - 1);
  }

  start() {
    if (!this.running) { this.running = true; requestAnimationFrame(this._raf); }
  }

  stop() { this.running = false; }

  // The (fractional) frame position to draw at time `now`: a forward sweep over 0..n.
  _position(now) {
    return ((now % this.loopMs) / this.loopMs) * this.n;
  }

  _tick(now) {
    if (!this.running) return;
    const { cv, ctx, n } = this;
    if (cv.width && cv.height) {
      const pos = this._position(now);
      const i = Math.min(Math.floor(pos), n - 1);
      // Ease the cross-fade with smoothstep instead of a linear ramp: at any
      // frame count the discrete keyframes blend with a soft S-curve so the
      // transition reads as continuous motion rather than a stepped dissolve.
      const frac = smoothstep(pos - Math.floor(pos));
      const next = (i + 1) % n;
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

// Smoothstep: 0→0, 1→1, with zero slope at both ends — eases the cross-fade so
// adjacent keyframes blend on an S-curve, killing the stepped look of a linear
// dissolve.
function smoothstep(t) {
  return t * t * (3 - 2 * t);
}
