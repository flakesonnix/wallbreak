// SPDX-License-Identifier: MIT
export const BUS_NAME = 'io.github.flakesonnix.Wallbreak';
export const OBJECT_PATH = '/io/github/flakesonnix/Wallbreak';
export const INTERFACE = 'io.github.flakesonnix.Wallbreak1';
export const APP_ID = 'io.github.flakesonnix.wallbreak.Renderer';
export const MAX_FRAME_BYTES = 64 * 1024 * 1024;
export const MAX_STDERR_BYTES = 32 * 1024;

export class BridgeError extends Error {
    constructor(code, message) {
        super(message);
        this.code = code;
    }
}

export function validateRequest([renderer, source, fps, fit, ffmpeg, output]) {
    const text = value => typeof value === 'string' && !value.includes('\0');
    if (!text(renderer) || !renderer.startsWith('/') ||
        renderer.split('/').at(-1) !== 'wallbreak-renderer')
        throw new BridgeError('InvalidArgument', 'Renderer must be an absolute wallbreak-renderer path');
    if (!text(source) || !source.startsWith('/'))
        throw new BridgeError('InvalidArgument', 'Source must be an absolute path');
    if (!Number.isInteger(fps) || fps < 1 || fps > 60)
        throw new BridgeError('InvalidArgument', 'FPS must be between 1 and 60');
    if (fit !== 'cover' && fit !== 'contain')
        throw new BridgeError('InvalidArgument', 'Fit must be cover or contain');
    if (!text(ffmpeg) || ffmpeg.length === 0)
        throw new BridgeError('InvalidArgument', 'FFmpeg must not be empty');
    if (!text(output))
        throw new BridgeError('InvalidArgument', 'Output must be a connector name or empty');
    return {renderer, source, fps, fit, ffmpeg, output};
}

export function frameGeometry(monitor, scale) {
    const width = monitor?.width * scale;
    const height = monitor?.height * scale;
    if (![monitor?.width, monitor?.height, width, height].every(
        value => Number.isSafeInteger(value) && value > 0) ||
        !Number.isFinite(scale) || scale <= 0 ||
        width > Math.floor(MAX_FRAME_BYTES / 4 / height))
        throw new BridgeError('InvalidArgument', 'Output frame must have integral dimensions and fit within 64 MiB');
    return {width, height, logicalWidth: monitor.width, logicalHeight: monitor.height};
}

export function rendererArgs(request, token, geometry) {
    return [request.renderer,
        '--source', request.source,
        '--session-token', token,
        '--width', String(geometry.width),
        '--height', String(geometry.height),
        '--logical-width', String(geometry.logicalWidth),
        '--logical-height', String(geometry.logicalHeight),
        '--fps', String(request.fps),
        '--fit', request.fit,
        '--ffmpeg', request.ffmpeg];
}

export function matchesRenderer({native, owned, pid, expectedPid, title, appId, token}) {
    return native === true && owned === true && Number.isSafeInteger(expectedPid) && expectedPid > 0 &&
        pid === expectedPid && title === `wallbreak-renderer:${token}` && appId === APP_ID;
}

// Keep bytes, not lines: a child may produce a single unbounded line or split UTF-8.
export class StderrTail {
    constructor() {
        this.bytes = new Uint8Array();
        this.truncated = false;
    }

    append(chunk) {
        const size = Math.min(MAX_STDERR_BYTES, this.bytes.length + chunk.length);
        const next = new Uint8Array(size);
        const fromChunk = Math.min(size, chunk.length);
        const fromOld = size - fromChunk;
        next.set(this.bytes.subarray(this.bytes.length - fromOld));
        next.set(chunk.subarray(chunk.length - fromChunk), fromOld);
        this.truncated ||= this.bytes.length + chunk.length > size;
        this.bytes = next;
    }

    text() {
        const text = new TextDecoder().decode(this.bytes).replaceAll('\0', '\\0');
        return `${this.truncated ? '[stderr truncated; last 32768 bytes]\n' : ''}${text}`;
    }
}
