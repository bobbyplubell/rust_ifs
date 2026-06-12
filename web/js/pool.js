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
  constructor(size) {
    this.size = size ?? Math.min(4, Math.max(1, (navigator.hardwareConcurrency || 4) - 1));
    this.queue = [];           // jobs waiting for a worker
    this.running = 0;
    this.chunksRendered = 0;   // session-wide chunk counter (progress msgs with a hash)
    this.onStats = null;       // assignable: ({size, queued, running, chunks}) => void
    this._nextJobId = 1;
    this._workers = [];
    for (let i = 0; i < this.size; i++) this._workers.push(this._spawn(i));
  }

  _spawn(index) {
    const worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
    const slot = { index, worker, ready: false, job: null };
    worker.onmessage = (event) => this._onMessage(slot, event.data);
    worker.onerror = (event) => {
      // A worker-level failure kills the current job but not the pool.
      if (slot.job) this._finish(slot, null, new Error(event.message || 'worker error'));
    };
    return slot;
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
    }
    this._stats();
  }

  _onMessage(slot, msg) {
    if (msg.type === 'ready') {
      slot.ready = true;
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
    if (job && !job.settled) {
      job.settled = true;
      if (error) job._reject(error);
      else job._resolve(result);
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
