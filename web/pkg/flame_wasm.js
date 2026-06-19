/**
 * A progressive, provable render: genome + spec + the running accumulation
 * buffer live in wasm memory. Each `render_chunk(idx)` renders chunk `idx`
 * into a temporary buffer, hashes it (the render-proof unit), merges it into
 * the running sum, and returns the hex hash. `tonemap()` can be called at any
 * point for the current progressive image.
 */
export class ChunkedRender {
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        ChunkedRenderFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_chunkedrender_free(ptr, 0);
    }
    /**
     * @returns {number}
     */
    chunks_done() {
        const ret = wasm.chunkedrender_chunks_done(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * @param {string} genome_json
     * @param {number} width
     * @param {number} height
     * @param {number} ss
     * @param {number} samples_per_chunk
     * @param {number} n_chunks
     * @param {string} challenge_hex
     */
    constructor(genome_json, width, height, ss, samples_per_chunk, n_chunks, challenge_hex) {
        const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(challenge_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.chunkedrender_new(ptr0, len0, width, height, ss, samples_per_chunk, n_chunks, ptr1, len1);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        this.__wbg_ptr = ret[0];
        ChunkedRenderFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * Render chunk `idx` into its own buffer, merge it into the running
     * accumulation, and return the chunk's hex hash.
     * @param {number} idx
     * @returns {string}
     */
    render_chunk(idx) {
        let deferred2_0;
        let deferred2_1;
        try {
            const ret = wasm.chunkedrender_render_chunk(this.__wbg_ptr, idx);
            var ptr1 = ret[0];
            var len1 = ret[1];
            if (ret[3]) {
                ptr1 = 0; len1 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred2_0 = ptr1;
            deferred2_1 = len1;
            return getStringFromWasm0(ptr1, len1);
        } finally {
            wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
        }
    }
    /**
     * Tone-map the current running accumulation to RGBA8 (`width*height*4`).
     * @returns {Uint8Array}
     */
    tonemap() {
        const ret = wasm.chunkedrender_tonemap(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
}
if (Symbol.dispose) ChunkedRender.prototype[Symbol.dispose] = ChunkedRender.prototype.free;

/**
 * One frame of a loop proof: hash (the proof unit), the tone-mapped RGBA
 * (rendering your proof doubles as watching the loop), and the raw integer
 * accumulation histogram (cells [r_fixed, g_fixed, b_fixed, count] u64,
 * row-major; reaches JS as a `BigUint64Array`) so frame histograms can be
 * summed into a cross-peer accumulated render.
 */
export class ProofFrame {
    static __wrap(ptr) {
        const obj = Object.create(ProofFrame.prototype);
        obj.__wbg_ptr = ptr;
        ProofFrameFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        ProofFrameFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_proofframe_free(ptr, 0);
    }
    /**
     * @returns {string}
     */
    get hash() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.proofframe_hash(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * @returns {BigUint64Array}
     */
    get hist() {
        const ret = wasm.proofframe_hist(this.__wbg_ptr);
        var v1 = getArrayU64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
    /**
     * @returns {Uint8Array}
     */
    get rgba() {
        const ret = wasm.proofframe_rgba(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
}
if (Symbol.dispose) ProofFrame.prototype[Symbol.dispose] = ProofFrame.prototype.free;

/**
 * One rendered batch: its content hash and its integer histogram.
 *
 * `hist` is the flat integer histogram (cells [r_fixed, g_fixed, b_fixed,
 * count] u64, row-major, length `w*ss*h*ss*4`) and reaches JS as a
 * `BigUint64Array` (zero float ambiguity, transferable). `hash` is the
 * lowercase hex of `sha256(hist LE bytes)` — the same bytes the histogram
 * serializes to, so JS can re-hash a merged histogram and get a matching id.
 */
export class RenderedBatch {
    static __wrap(ptr) {
        const obj = Object.create(RenderedBatch.prototype);
        obj.__wbg_ptr = ptr;
        RenderedBatchFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        RenderedBatchFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_renderedbatch_free(ptr, 0);
    }
    /**
     * @returns {string}
     */
    get hash() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.renderedbatch_hash(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * @returns {BigUint64Array}
     */
    get hist() {
        const ret = wasm.renderedbatch_hist(this.__wbg_ptr);
        var v1 = getArrayU64FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 8, 8);
        return v1;
    }
}
if (Symbol.dispose) RenderedBatch.prototype[Symbol.dispose] = RenderedBatch.prototype.free;

/**
 * Re-render one chunk and return its hex hash without keeping any pixels —
 * the audit primitive (1/n_chunks of a render's cost).
 * @param {string} genome_json
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @param {number} samples_per_chunk
 * @param {string} challenge_hex
 * @param {number} idx
 * @returns {string}
 */
export function audit_chunk(genome_json, width, height, ss, samples_per_chunk, challenge_hex, idx) {
    let deferred4_0;
    let deferred4_1;
    try {
        const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(challenge_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.audit_chunk(ptr0, len0, width, height, ss, samples_per_chunk, ptr1, len1, idx);
        var ptr3 = ret[0];
        var len3 = ret[1];
        if (ret[3]) {
            ptr3 = 0; len3 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred4_0 = ptr3;
        deferred4_1 = len3;
        return getStringFromWasm0(ptr3, len3);
    } finally {
        wasm.__wbindgen_free(deferred4_0, deferred4_1, 1);
    }
}

/**
 * Audit one loop-proof frame: recompute its hash only (no pixels kept).
 * @param {string} genome_json
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @param {number} samples_per_frame
 * @param {string} challenge_hex
 * @param {number} idx
 * @param {number} n_frames
 * @param {number} temporal
 * @returns {string}
 */
export function audit_frame(genome_json, width, height, ss, samples_per_frame, challenge_hex, idx, n_frames, temporal) {
    let deferred4_0;
    let deferred4_1;
    try {
        const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(challenge_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.audit_frame(ptr0, len0, width, height, ss, samples_per_frame, ptr1, len1, idx, n_frames, temporal);
        var ptr3 = ret[0];
        var len3 = ret[1];
        if (ret[3]) {
            ptr3 = 0; len3 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred4_0 = ptr3;
        deferred4_1 = len3;
        return getStringFromWasm0(ptr3, len3);
    } finally {
        wasm.__wbindgen_free(deferred4_0, deferred4_1, 1);
    }
}

/**
 * Audit primitive: re-render batch `(frame, idx)` and return ONLY its content
 * hash (no histogram kept). Same determinism as `render_batch`.
 * @param {string} genome_json
 * @param {string} sheep_id_hex
 * @param {number} frame
 * @param {number} idx
 * @param {number} w
 * @param {number} h
 * @param {number} ss
 * @param {number} spp
 * @param {number} n_frames
 * @returns {string}
 */
export function batch_hash(genome_json, sheep_id_hex, frame, idx, w, h, ss, spp, n_frames) {
    let deferred4_0;
    let deferred4_1;
    try {
        const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(sheep_id_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.batch_hash(ptr0, len0, ptr1, len1, frame, idx, w, h, ss, spp, n_frames);
        var ptr3 = ret[0];
        var len3 = ret[1];
        if (ret[3]) {
            ptr3 = 0; len3 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred4_0 = ptr3;
        deferred4_1 = len3;
        return getStringFromWasm0(ptr3, len3);
    } finally {
        wasm.__wbindgen_free(deferred4_0, deferred4_1, 1);
    }
}

/**
 * Deterministically breed two genomes. The rng seed is the first 8 bytes
 * (little-endian) of the decoded 32-byte challenge; mutation rate is 0.15.
 * Returns the child's canonical JSON.
 * @param {string} a_json
 * @param {string} b_json
 * @param {string} challenge_hex
 * @returns {string}
 */
export function breed(a_json, b_json, challenge_hex) {
    let deferred5_0;
    let deferred5_1;
    try {
        const ptr0 = passStringToWasm0(a_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(b_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ptr2 = passStringToWasm0(challenge_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len2 = WASM_VECTOR_LEN;
        const ret = wasm.breed(ptr0, len0, ptr1, len1, ptr2, len2);
        var ptr4 = ret[0];
        var len4 = ret[1];
        if (ret[3]) {
            ptr4 = 0; len4 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred5_0 = ptr4;
        deferred5_1 = len4;
        return getStringFromWasm0(ptr4, len4);
    } finally {
        wasm.__wbindgen_free(deferred5_0, deferred5_1, 1);
    }
}

/**
 * Re-serialize genome JSON into its canonical byte form.
 * @param {string} genome_json
 * @returns {string}
 */
export function canonicalize(genome_json) {
    let deferred3_0;
    let deferred3_1;
    try {
        const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.canonicalize(ptr0, len0);
        var ptr2 = ret[0];
        var len2 = ret[1];
        if (ret[3]) {
            ptr2 = 0; len2 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred3_0 = ptr2;
        deferred3_1 = len2;
        return getStringFromWasm0(ptr2, len2);
    } finally {
        wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
    }
}

/**
 * Convenience challenge for casual (non-proof) renders:
 * `sha256(le64(seed))`, returned as lowercase hex.
 * @param {number} seed
 * @returns {string}
 */
export function challenge_from_seed(seed) {
    let deferred1_0;
    let deferred1_1;
    try {
        const ret = wasm.challenge_from_seed(seed);
        deferred1_0 = ret[0];
        deferred1_1 = ret[1];
        return getStringFromWasm0(ret[0], ret[1]);
    } finally {
        wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
    }
}

/**
 * Mutate a genome with the given per-site rate, seeded from the challenge
 * like `breed`. Returns the mutant's canonical JSON.
 * @param {string} genome_json
 * @param {string} challenge_hex
 * @param {number} rate
 * @returns {string}
 */
export function mutate_genome(genome_json, challenge_hex, rate) {
    let deferred4_0;
    let deferred4_1;
    try {
        const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ptr1 = passStringToWasm0(challenge_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len1 = WASM_VECTOR_LEN;
        const ret = wasm.mutate_genome(ptr0, len0, ptr1, len1, rate);
        var ptr3 = ret[0];
        var len3 = ret[1];
        if (ret[3]) {
            ptr3 = 0; len3 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred4_0 = ptr3;
        deferred4_1 = len3;
        return getStringFromWasm0(ptr3, len3);
    } finally {
        wasm.__wbindgen_free(deferred4_0, deferred4_1, 1);
    }
}

/**
 * The animation frame count of a sheep's loop (`N_FRAMES`), exposed so JS uses
 * the same constant the renderer does.
 * @returns {number}
 */
export function n_frames() {
    const ret = wasm.n_frames();
    return ret >>> 0;
}

/**
 * @param {string} genome_json
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @param {number} samples_per_frame
 * @param {string} challenge_hex
 * @param {number} idx
 * @param {number} n_frames
 * @param {number} temporal
 * @returns {ProofFrame}
 */
export function proof_frame(genome_json, width, height, ss, samples_per_frame, challenge_hex, idx, n_frames, temporal) {
    const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passStringToWasm0(challenge_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.proof_frame(ptr0, len0, width, height, ss, samples_per_frame, ptr1, len1, idx, n_frames, temporal);
    if (ret[2]) {
        throw takeFromExternrefTable0(ret[1]);
    }
    return ProofFrame.__wrap(ret[0]);
}

/**
 * A random genome (same generator as `flame dump`), as canonical JSON.
 * @param {number} seed
 * @param {number} transforms
 * @returns {string}
 */
export function random_genome_json(seed, transforms) {
    let deferred2_0;
    let deferred2_1;
    try {
        const ret = wasm.random_genome_json(seed, transforms);
        var ptr1 = ret[0];
        var len1 = ret[1];
        if (ret[3]) {
            ptr1 = 0; len1 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred2_0 = ptr1;
        deferred2_1 = len1;
        return getStringFromWasm0(ptr1, len1);
    } finally {
        wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
    }
}

/**
 * Render batch `(frame, idx)` of the sheep identified by `sheep_id_hex`
 * (32-byte hex). The genome is animated to `phase = frame / n_frames`, then
 * `spp` samples are plotted from `batch_seed(sheep_id, frame, idx)` into an
 * integer histogram at `w*ss x h*ss`. `n_frames` is the sheep's loop length
 * (from its spec) so a 128-frame sheep renders phase = frame / 128.
 * Deterministic: every peer rendering the same args gets a byte-identical
 * `hist` and `hash`.
 * @param {string} genome_json
 * @param {string} sheep_id_hex
 * @param {number} frame
 * @param {number} idx
 * @param {number} w
 * @param {number} h
 * @param {number} ss
 * @param {number} spp
 * @param {number} n_frames
 * @returns {RenderedBatch}
 */
export function render_batch(genome_json, sheep_id_hex, frame, idx, w, h, ss, spp, n_frames) {
    const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passStringToWasm0(sheep_id_hex, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.render_batch(ptr0, len0, ptr1, len1, frame, idx, w, h, ss, spp, n_frames);
    if (ret[2]) {
        throw takeFromExternrefTable0(ret[1]);
    }
    return RenderedBatch.__wrap(ret[0]);
}

/**
 * Render one animation frame: the genome at loop `phase` (0..1) — flam3-style
 * transform-basis rotation plus palette drift, with temporal samples (motion
 * blur): the budget is split over `temporal` sub-phases spanning `shutter`
 * loop-phase units (temporal <= 1 or shutter <= 0 = single instant).
 * Display-only; proofs always render the base genome.
 * @param {string} genome_json
 * @param {number} phase
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @param {number} samples
 * @param {number} seed
 * @param {number} shutter
 * @param {number} temporal
 * @param {number} directional
 * @returns {Uint8Array}
 */
export function render_frame(genome_json, phase, width, height, ss, samples, seed, shutter, temporal, directional) {
    const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.render_frame(ptr0, len0, phase, width, height, ss, samples, seed, shutter, temporal, directional);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v2;
}

/**
 * Render a genome (as JSON) to an RGBA8 byte buffer (`width*height*4`), ready
 * to drop into a canvas `ImageData`.
 *
 * `rotate` is added to the camera angle so the gallery can animate a spin by
 * calling this each frame with an increasing value.
 * @param {string} genome_json
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @param {number} samples
 * @param {number} seed
 * @param {number} rotate
 * @returns {Uint8Array}
 */
export function render_rgba(genome_json, width, height, ss, samples, seed, rotate) {
    const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.render_rgba(ptr0, len0, width, height, ss, samples, seed, rotate);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v2;
}

/**
 * SHA-256 of the canonical genome JSON, as lowercase hex.
 * @param {string} genome_json
 * @returns {string}
 */
export function sheep_id(genome_json) {
    let deferred3_0;
    let deferred3_1;
    try {
        const ptr0 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.sheep_id(ptr0, len0);
        var ptr2 = ret[0];
        var len2 = ret[1];
        if (ret[3]) {
            ptr2 = 0; len2 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred3_0 = ptr2;
        deferred3_1 = len2;
        return getStringFromWasm0(ptr2, len2);
    } finally {
        wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
    }
}

export function start() {
    wasm.start();
}

/**
 * Verification helper: `true` iff subtracting integer histogram `batch` from
 * `acc` underflows no channel (confirms `batch ⊆ acc`). Both are
 * `BigUint64Array` of the same dimensions.
 * @param {BigUint64Array} acc
 * @param {BigUint64Array} batch
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @returns {boolean}
 */
export function subtract_check(acc, batch, width, height, ss) {
    const ptr0 = passArray64ToWasm0(acc, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passArray64ToWasm0(batch, wasm.__wbindgen_malloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.subtract_check(ptr0, len0, ptr1, len1, width, height, ss);
    if (ret[2]) {
        throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] !== 0;
}

/**
 * Tone-map a raw INTEGER histogram (cells [r_fixed, g_fixed, b_fixed, count]
 * u64, row-major at `w*ss x h*ss`, passed from JS as a `BigUint64Array`) —
 * used to display cross-peer ACCUMULATED renders: verified summed integer
 * histograms from many contributors' batches, tonemapped locally.
 *
 * (Integer-era replacement for the old float `tonemap_hist`; the histogram
 * layout matches `render_batch().hist` and `total_count`/`subtract_check`.)
 * @param {BigUint64Array} hist
 * @param {string} genome_json
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @returns {Uint8Array}
 */
export function tonemap_hist_int(hist, genome_json, width, height, ss) {
    const ptr0 = passArray64ToWasm0(hist, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passStringToWasm0(genome_json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.tonemap_hist_int(ptr0, len0, ptr1, len1, width, height, ss);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v3 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v3;
}

/**
 * Verification helper: total `count` over all cells of an integer histogram
 * (the count-conservation left side). `hist` is a `BigUint64Array`.
 * @param {BigUint64Array} hist
 * @param {number} width
 * @param {number} height
 * @param {number} ss
 * @returns {bigint}
 */
export function total_count(hist, width, height, ss) {
    const ptr0 = passArray64ToWasm0(hist, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.total_count(ptr0, len0, width, height, ss);
    if (ret[2]) {
        throw takeFromExternrefTable0(ret[1]);
    }
    return BigInt.asUintN(64, ret[0]);
}
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg___wbindgen_throw_ea4887a5f8f9a9db: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbg_error_a6fa202b58aa1cd3: function(arg0, arg1) {
            let deferred0_0;
            let deferred0_1;
            try {
                deferred0_0 = arg0;
                deferred0_1 = arg1;
                console.error(getStringFromWasm0(arg0, arg1));
            } finally {
                wasm.__wbindgen_free(deferred0_0, deferred0_1, 1);
            }
        },
        __wbg_new_227d7c05414eb861: function() {
            const ret = new Error();
            return ret;
        },
        __wbg_stack_3b0d974bbf31e44f: function(arg0, arg1) {
            const ret = arg1.stack;
            const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            const len1 = WASM_VECTOR_LEN;
            getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
            getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
        },
        __wbindgen_cast_0000000000000001: function(arg0, arg1) {
            // Cast intrinsic for `Ref(String) -> Externref`.
            const ret = getStringFromWasm0(arg0, arg1);
            return ret;
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            const offset = table.grow(4);
            table.set(0, undefined);
            table.set(offset + 0, undefined);
            table.set(offset + 1, null);
            table.set(offset + 2, true);
            table.set(offset + 3, false);
        },
    };
    return {
        __proto__: null,
        "./flame_wasm_bg.js": import0,
    };
}

const ChunkedRenderFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_chunkedrender_free(ptr, 1));
const ProofFrameFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_proofframe_free(ptr, 1));
const RenderedBatchFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_renderedbatch_free(ptr, 1));

function getArrayU64FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getBigUint64ArrayMemory0().subarray(ptr / 8, ptr / 8 + len);
}

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

let cachedBigUint64ArrayMemory0 = null;
function getBigUint64ArrayMemory0() {
    if (cachedBigUint64ArrayMemory0 === null || cachedBigUint64ArrayMemory0.byteLength === 0) {
        cachedBigUint64ArrayMemory0 = new BigUint64Array(wasm.memory.buffer);
    }
    return cachedBigUint64ArrayMemory0;
}

let cachedDataViewMemory0 = null;
function getDataViewMemory0() {
    if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
        cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
    }
    return cachedDataViewMemory0;
}

function getStringFromWasm0(ptr, len) {
    return decodeText(ptr >>> 0, len);
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function passArray64ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 8, 8) >>> 0;
    getBigUint64ArrayMemory0().set(arg, ptr / 8);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

function takeFromExternrefTable0(idx) {
    const value = wasm.__wbindgen_externrefs.get(idx);
    wasm.__externref_table_dealloc(idx);
    return value;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedBigUint64ArrayMemory0 = null;
    cachedDataViewMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('flame_wasm_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
