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
    this._raf = (t) => this._tick(t);
  }

  start() {
    if (!this.running) { this.running = true; requestAnimationFrame(this._raf); }
  }

  stop() { this.running = false; }

  _tick(now) {
    if (!this.running) return;
    const { cv, ctx, n } = this;
    if (cv.width && cv.height) {
      const pos = ((now % this.loopMs) / this.loopMs) * n;
      const i = Math.floor(pos) % n;
      const frac = pos - Math.floor(pos);
      const a = this.getFrame(i) || null;
      const b = this.getFrame((i + 1) % n) || a; // hold on a if the next isn't ready
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
