// pool.js — a small Web Worker pool over worker.js.
//
// One job per worker at a time, FIFO queue for the rest. submit() returns a
// job handle:
//   handle.id          job id (string)
//   handle.onProgress  assignable callback, receives raw `progress` messages
//   handle.done        promise: resolves with the final `done` / `breed-done`
//                      message (or {type:'cancelled'}), rejects on `error`
//   handle.cancel()    cancels: dequeues if still queued, otherwise posts
//                      {type:'cancel'} to the owning worker; resolves `done`
//                      with {type:'cancelled'} either way.

export class WorkerPool {
  constructor(size, opts = {}) {
    // Rendering is embarrassingly parallel across batches, so more workers =
    // proportionally faster accumulation. The old flat cap of 4 left cores idle
    // on capable machines. Raise the cap only where it's safe: gate on
    // deviceMemory so we don't cook phones / thrash low-RAM laptops (each worker
    // holds its own wasm instance). deviceMemory is Chromium-only; Firefox/Safari
    // report undefined -> fall back to the old conservative cap of 4.
    this.size = size ?? (() => {
      const cores = navigator.hardwareConcurrency || 4;
      const mem = navigator.deviceMemory || 4; // GB; undefined -> 4 (conservative)
      const cap = mem >= 8 ? 8 : 4;            // beefy box -> up to 8, else stay at 4
      return Math.max(1, Math.min(cap, cores - 1));
    })();
    this.queue = [];           // jobs waiting for a worker
    this.running = 0;
    this.chunksRendered = 0;   // session-wide chunk counter (progress msgs with a hash)
    this.onStats = null;       // assignable: ({size, queued, running, chunks}) => void
    this._nextJobId = 1;

    // Load-robustness: a worker that never finishes its wasm init (flaky
    // cellular drops the fetch/compile) never posts {type:'ready'}, so the
    // pool would silently sit dead forever. We arm a per-slot READY timeout and
    // also treat worker.onerror-before-ready as a load failure, then respawn
    // with capped exponential backoff. If every slot exhausts its attempts with
    // none ready, we flip status to 'failed' so the UI can stop being silent.
    this.status = 'loading';   // 'loading' | 'ready' | 'failed'
    this.onStatus = null;      // assignable: (status, detail) => void
    this._readyTimeoutMs = 20_000;
    this._maxAttempts = 5;     // respawn attempts per slot before giving up
    this._backoffMs = [500, 1000, 2000, 4000, 8000];
    // Per-job render timeout. A pathological tile can drive the chaos game into a
    // non-terminating path; the worker is then stuck in a SYNCHRONOUS wasm loop
    // and will never process a {type:'cancel'} message, so `handle.done` would
    // hang forever and freeze a caller that awaits it (e.g. the contribute loop).
    // We bound it: terminate + respawn the stuck worker and settle the job as
    // {type:'timeout'} so the caller moves on. Generous so a slow phone rendering
    // a legit 200k-sample tile is never killed.
    this._jobTimeoutMs = opts.jobTimeoutMs ?? 20_000;

    this._workers = new Array(this.size);
    for (let i = 0; i < this.size; i++) this._spawn(i);
  }

  // (Re)spawn the worker for a slot index, in place. Wrapped in try/catch
  // because `new Worker(...)` itself can throw on some mobile browsers / under
  // a strict CSP — that's a load failure, not a runtime one.
  _spawn(index) {
    const prev = this._workers[index];
    const attempts = prev ? prev.attempts : 0;
    const slot = { index, worker: null, ready: false, job: null, attempts, readyTimer: null };
    this._workers[index] = slot;

    let worker;
    try {
      worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
    } catch (err) {
      // Couldn't even construct the worker — count it as a failed attempt.
      this._workerFailed(slot, `worker construction failed: ${err?.message || err}`);
      return slot;
    }
    slot.worker = worker;
    worker.onmessage = (event) => this._onMessage(slot, event.data);
    worker.onerror = (event) => {
      if (slot.job) {
        // Runtime failure mid-job: kill that job, but the worker (and pool)
        // survive — unchanged happy-path behavior.
        this._finish(slot, null, new Error(event.message || 'worker error'));
      } else if (!slot.ready) {
        // Failure before the worker ever readied (e.g. wasm fetch/compile
        // blew up): treat as a load failure and respawn.
        this._workerFailed(slot, `worker load error: ${event.message || 'unknown'}`);
      }
    };

    // Arm the ready timeout: a worker that never posts {type:'ready'} is
    // indistinguishable (to us) from a dead one. _onMessage clears this on ready.
    slot.readyTimer = setTimeout(() => {
      slot.readyTimer = null;
      if (!slot.ready) this._workerFailed(slot, 'worker init timed out');
    }, this._readyTimeoutMs);

    return slot;
  }

  // Handle a slot that failed to load/init. Re-queue any in-flight job so it
  // isn't lost, terminate the dead worker, and either respawn with backoff or
  // give up on this slot. Re-evaluates pool status after.
  _workerFailed(slot, reason) {
    if (slot.readyTimer) { clearTimeout(slot.readyTimer); slot.readyTimer = null; }

    // If a job was in flight on this slot, put it back at the front of the
    // queue so another (or the respawned) worker can pick it up.
    if (slot.job) {
      const job = slot.job;
      slot.job = null;
      job.slot = null;
      this.running--;
      if (!job.settled) this.queue.unshift(job);
    }

    if (slot.worker) {
      try { slot.worker.terminate(); } catch { /* already gone */ }
      slot.worker = null;
    }

    slot.attempts++;
    if (slot.attempts < this._maxAttempts) {
      const delay = this._backoffMs[Math.min(slot.attempts - 1, this._backoffMs.length - 1)];
      setTimeout(() => {
        // Only respawn if the slot is still the dead one (defensive).
        if (this._workers[slot.index] === slot && !slot.ready) this._spawn(slot.index);
      }, delay);
    }
    // else: this slot has given up. _evalStatus decides if the WHOLE pool failed.

    this._evalStatus(reason);
  }

  // Recompute pool status and fire onStatus on transition.
  //   ready  — at least one worker is ready
  //   failed — every slot has exhausted its respawn attempts and none is ready
  //   loading — otherwise (still trying)
  _evalStatus(reason) {
    let next;
    if (this._workers.some((s) => s && s.ready)) {
      next = 'ready';
    } else if (this._workers.every((s) => s && s.attempts >= this._maxAttempts && !s.ready)) {
      next = 'failed';
    } else {
      next = 'loading';
    }
    if (next === this.status) return;
    // 'ready' is sticky: once any worker has loaded, the pool stays usable even
    // if some sibling slots later give up.
    if (this.status === 'ready' && next !== 'ready') return;
    this.status = next;
    if (this.onStatus) {
      this.onStatus(next, next === 'failed' ? (reason || 'renderer failed to load') : reason);
    }
  }

  /** Submit a message to the pool. jobId is assigned here. */
  submit(message, { onProgress } = {}) {
    const id = `job-${this._nextJobId++}`;
    const job = { id, message: { ...message, jobId: id }, slot: null, onProgress: onProgress || null };
    job.done = new Promise((resolve, reject) => {
      job._resolve = resolve;
      job._reject = reject;
    });

    const handle = {
      id,
      done: job.done,
      set onProgress(fn) { job.onProgress = fn; },
      get onProgress() { return job.onProgress; },
      cancel: () => this._cancel(job),
    };
    job.handle = handle;

    this.queue.push(job);
    this._pump();
    return handle;
  }

  _cancel(job) {
    if (job.settled) return;
    const queuedAt = this.queue.indexOf(job);
    if (queuedAt !== -1) {
      this.queue.splice(queuedAt, 1);
      job.settled = true;
      job._resolve({ type: 'cancelled', jobId: job.id });
      this._stats();
      return;
    }
    if (job.slot) {
      job.slot.worker.postMessage({ type: 'cancel', jobId: job.id });
      // The worker posts nothing more for a cancelled job (it bails at the
      // next chunk boundary), so settle and free the slot here.
      this._finish(job.slot, { type: 'cancelled', jobId: job.id }, null);
    }
  }

  _pump() {
    for (const slot of this._workers) {
      if (!slot.ready || slot.job) continue;
      const job = this.queue.shift();
      if (!job) break;
      slot.job = job;
      job.slot = slot;
      this.running++;
      slot.worker.postMessage(job.message);
      // Bound this job's wall-clock so a hung render can't freeze the caller.
      job._timer = setTimeout(() => this._onJobTimeout(slot, job), this._jobTimeoutMs);
    }
    this._stats();
  }

  // A job exceeded its render budget — almost certainly a worker stuck in a
  // synchronous wasm loop (which can't honor a cancel message). Terminate +
  // respawn the worker and settle the job as {type:'timeout'} so the awaiting
  // caller continues. (NOT re-queued — re-running the same hanging tile would
  // just hang again.)
  _onJobTimeout(slot, job) {
    if (job.settled || slot.job !== job) return;
    job._timer = null;
    job.settled = true;
    job.slot = null;
    this.running--;
    slot.job = null;
    if (slot.worker) { try { slot.worker.terminate(); } catch { /* already gone */ } slot.worker = null; }
    job._resolve({ type: 'timeout', jobId: job.id });
    // Fresh worker in this slot (attempts reset — a job hang isn't a load fault).
    slot.attempts = 0;
    this._spawn(slot.index);
    this._stats();
  }

  _onMessage(slot, msg) {
    if (msg.type === 'ready') {
      if (slot.readyTimer) { clearTimeout(slot.readyTimer); slot.readyTimer = null; }
      slot.ready = true;
      this._evalStatus();
      this._pump();
      return;
    }
    const job = slot.job;
    if (!job || msg.jobId !== job.id) return; // stale (e.g. post-cancel stragglers)

    switch (msg.type) {
      case 'progress':
        if (msg.hash) {
          this.chunksRendered++;
          this._stats();
        }
        if (job.onProgress) job.onProgress(msg);
        break;
      case 'done':
      case 'breed-done':
      case 'batch-done':
        this._finish(slot, msg, null);
        break;
      case 'error':
        this._finish(slot, null, new Error(msg.message));
        break;
    }
  }

  _finish(slot, result, error) {
    const job = slot.job;
    slot.job = null;
    this.running--;
    if (job) {
      if (job._timer) { clearTimeout(job._timer); job._timer = null; }
      if (!job.settled) {
        job.settled = true;
        if (error) job._reject(error);
        else job._resolve(result);
      }
    }
    this._pump();
  }

  _stats() {
    if (this.onStats) {
      this.onStats({
        size: this.size,
        queued: this.queue.length,
        running: this.running,
        chunks: this.chunksRendered,
      });
    }
  }
}
