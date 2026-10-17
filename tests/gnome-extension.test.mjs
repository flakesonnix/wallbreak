// Run: node --experimental-vm-modules --test tests/gnome-extension.test.mjs
// Shell/GI boundaries are simulated; this does not verify a live compositor.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

import {
    APP_ID, BUS_NAME, INTERFACE, OBJECT_PATH, MAX_FRAME_BYTES, MAX_STDERR_BYTES,
    validateRequest, frameGeometry, rendererArgs, matchesRenderer, StderrTail,
} from '../extensions/wallbreak@flakesonnix.github.io/protocol.mjs';

const root = new URL('../extensions/wallbreak@flakesonnix.github.io/', import.meta.url);
const [extensionSource, protocolSource, xml, metadata] = await Promise.all([
    readFile(new URL('extension.js', root), 'utf8'),
    readFile(new URL('protocol.mjs', root), 'utf8'),
    readFile(new URL('dbus.xml', root), 'utf8'),
    readFile(new URL('metadata.json', root), 'utf8').then(JSON.parse),
]);
const request = ['/opt/wall break/wallbreak-renderer', '/media/a;$(touch nope).mp4', 30, 'cover', 'ffmpeg', ''];
const flush = async () => {
    for (let i = 0; i < 20; i++)
        await Promise.resolve();
};

class Signals {
    handlers = new Map();
    next = 1;
    connect(name, callback) {
        const id = this.next++;
        this.handlers.set(id, {name, callback});
        return id;
    }
    disconnect(id) {
        assert.ok(this.handlers.delete(id), `disconnect of unknown signal ${id}`);
    }
    emit(name, ...args) {
        for (const signal of [...this.handlers.values()]) {
            if (signal.name === name)
                signal.callback(this, ...args);
        }
    }
}

class Actor extends Signals {
    opacity = 255;
    children = [];
    visible = true;
    constructor(properties = {}) {
        super();
        Object.assign(this, properties);
    }
    add_child(actor) {
        this.children.push(actor);
        actor.parent = this;
    }
    set_clip(...clip) { this.clip = clip; }
    destroy() {
        if (this.destroyed)
            throw new Error('Actor already destroyed');
        this.destroyed = true;
        for (const child of [...this.children])
            child.destroy();
        if (this.parent)
            this.parent.children = this.parent.children.filter(child => child !== this);
        this.emit('destroy');
    }
}

class Window extends Signals {
    actor = new Actor();
    native = true;
    owned = true;
    pid = 700;
    title = null;
    appId = null;
    touched = [];
    inhibitors = 0;
    get_client_type() { return this.native ? 1 : 0; }
    get_pid() { return this.pid; }
    get_title() { return this.title; }
    get_wm_class() { return this.appId; }
    get_compositor_private() { return this.actor; }
    set_type(type) { this.touched.push(['type', type]); }
    hide_from_window_list() { this.touched.push(['hide']); }
    inhibit_mapped() {
        assert.ok(this.hasShown, 'must not inhibit before first shown');
        this.inhibitors++;
    }
    uninhibit_mapped() {
        assert.equal(this.inhibitors, 1);
        this.inhibitors--;
    }
    shown() {
        this.hasShown = true;
        this.emit('shown');
    }
}

class Cancel {
    cancelled = false;
    cancel() {
        this.cancelled = true;
        this.onCancel?.();
    }
    is_cancelled() { return this.cancelled; }
}

class Stream {
    queue = [];
    read_bytes_async(size, _priority, cancel, callback) {
        if (this.scheduleError)
            throw new Error('read scheduling failed');
        assert.equal(size, 4096);
        assert.equal(this.pending, undefined, 'only one pending read');
        this.pending = callback;
        cancel.onCancel = () => this.deliver(new Error('cancelled'));
        this.pump();
    }
    read_bytes_finish(result) {
        if (result instanceof Error)
            throw result;
        return {get_data: () => result};
    }
    deliver(result) {
        if (!this.pending)
            return;
        const callback = this.pending;
        this.pending = undefined;
        queueMicrotask(() => callback(this, result));
    }
    pump() {
        if (this.pending && this.queue.length)
            this.deliver(this.queue.shift());
    }
    feed(bytes) {
        for (let offset = 0; offset < bytes.length; offset += 4096)
            this.queue.push(bytes.subarray(offset, offset + 4096));
        this.pump();
    }
    end() {
        this.queue.push(new Uint8Array());
        this.pump();
    }
    close_async(_priority, _cancel, callback) {
        queueMicrotask(() => callback(this, {}));
    }
    close_finish() {
        if (this.closeError)
            throw new Error('close failed');
        this.closed = true;
    }
    is_closed() { return this.closed === true; }
}

class Process {
    stream = new Stream();
    sent = [];
    wait_async(_cancel, callback) {
        if (this.scheduleError)
            throw new Error('wait scheduling failed');
        this.wait = callback;
    }
    wait_finish(result) {
        if (result instanceof Error)
            throw result;
        assert.ok(this.exited);
    }
    get_stderr_pipe() { return this.stream; }
    send_signal(signal) { this.sent.push(signal); }
    force_exit() { this.sent.push(9); }
    get_if_exited() { return !this.signal; }
    get_exit_status() { return this.code; }
    get_term_sig() { return this.signal; }
    exit(code = 0, {signal = 0, eof = true} = {}) {
        this.exited = true;
        this.code = code;
        this.signal = signal;
        if (eof)
            this.stream.end();
        queueMicrotask(() => this.wait(this, {}));
    }
}

async function harness({sessionType = 'wayland'} = {}) {
    const timers = new Map();
    const watches = new Map();
    const busOwners = new Map();
    const spawned = [];
    const warnings = [];
    const display = new Signals();
    const overview = Object.assign(new Signals(), {visible: false, visibleTarget: false});
    const sessionMode = Object.assign(new Signals(), {hasWindows: true, isLocked: false});
    const manager = Object.assign(new Signals(), {backgroundActor: new Actor()});
    const layoutManager = Object.assign(new Signals(), {
        primaryIndex: 0, monitors: [{width: 1920, height: 1080}], _bgManagers: [manager],
        screenShieldGroup: Object.assign(new Signals(), {visible: false}),
    });
    let id = 0;
    let now = 0;
    let sequence = 0;
    const h = {timers, watches, busOwners, spawned, warnings, display, overview,
        sessionMode, manager, layoutManager, scale: 1, overviewOwners: []};
    display.get_monitor_scale = () => h.scale;
    const global = {display, context: {}, backend: {get_monitor_manager: () => ({
        get_monitor_for_connector: output => output === 'DP-1' ? 0 : -1,
    })}};
    const GLib = {
        getenv: name => name === 'XDG_SESSION_TYPE' ? sessionType : null,
        PRIORITY_DEFAULT: 0, SOURCE_REMOVE: false,
        uuid_string_random: () => `00000000-0000-4000-8000-${String(++sequence).padStart(12, '0')}`,
        Variant: class {
            constructor(signature, value) { Object.assign(this, {signature, value}); }
        },
        timeout_add: (_priority, ms, callback) => {
            const key = ++id;
            timers.set(key, {at: now + ms, callback});
            return key;
        },
        source_remove: key => timers.delete(key),
    };
    const Gio = {
        DBus: {session: {}}, FileQueryInfoFlags: {NONE: 0}, FileType: {REGULAR: 1},
        BusNameOwnerFlags: {DO_NOT_QUEUE: 4}, BusNameWatcherFlags: {NONE: 0},
        SubprocessFlags: {STDOUT_SILENCE: 1, STDERR_PIPE: 2}, Cancellable: Cancel,
        File: {new_for_path: path => ({query_info: () => {
            h.queriedPath = path;
            if (h.fileError)
                throw new Error('file does not exist');
            return {get_file_type: () => h.directory ? 2 : 1,
                get_attribute_boolean: () => !h.notExecutable};
        }})},
        DBusExportedObject: {wrapJSObject: (definition, receiver) => {
            assert.equal(definition, xml, 'canonical XML must be loaded at runtime');
            const exported = {
                export: (_connection, path) => { h.exportedPath = path; },
                unexport: () => { exported.closed = true; },
            };
            h.exported = exported;
            h.receiver = receiver;
            return exported;
        }},
        bus_own_name_on_connection: (_connection, name, flags, acquired, lost) => {
            assert.equal(name, BUS_NAME);
            assert.equal(flags, 4, 'must not replace another service or queue');
            const key = ++id;
            busOwners.set(key, {acquired, lost});
            return key;
        },
        bus_unown_name: key => busOwners.delete(key),
        bus_watch_name_on_connection: (_connection, name, _flags, _appeared, vanished) => {
            const key = ++id;
            watches.set(key, {name, vanished});
            return key;
        },
        bus_unwatch_name: key => watches.delete(key),
        SubprocessLauncher: class {
            constructor(options) { h.launchOptions = options; }
            close() { h.launcherClosed = true; }
        },
    };
    const Meta = {
        WindowClientType: {WAYLAND: 1}, WindowType: {UTILITY: 4},
        WaylandClient: {new_subprocess: (context, _launcher, argv) => {
            assert.equal(context, global.context);
            assert.ok([...display.handlers.values()].some(signal => signal.name === 'window-created'));
            if (h.spawnError)
                throw new Error('spawn denied');
            const process = new Process();
            process.scheduleError = h.waitScheduleError;
            process.stream.scheduleError = h.readScheduleError;
            const client = {get_subprocess: () => process, get_pid: () => 700,
                owns_window: window => window.owned};
            spawned.push({argv: Array.from(argv), process, client});
            return client;
        }},
    };
    class Extension {
        dir = {get_child: name => {
            assert.equal(name, 'dbus.xml');
            return {load_contents: () => [true, new TextEncoder().encode(xml)]};
        }};
    }
    // The overview owner has its own full-module tests. Here verify that the
    // session passes ownership/privacy facts and waits for its cleanup.
    class OverviewClones {
        constructor(source, monitor, index, onError) {
            Object.assign(this, {source, monitor, index, onError, hasResources: false});
            h.overviewOwners.push(this);
        }
        start() {
            this.hasResources = true;
            if (h.overviewStartError)
                this.onError(new Error('overview startup failed'));
        }
        setVisibility(allowed, overviewVisible) {
            Object.assign(this, {allowed, overviewVisible});
        }
        destroy() {
            if (h.overviewCleanupError)
                throw new Error('overview cleanup failed');
            Object.assign(this, {hasResources: false, destroyed: true, source: null, onError: null});
        }
    }
    const context = vm.createContext({
        global, TextDecoder, TextEncoder, Uint8Array, console: {error: text => warnings.push(text)},
    });
    const modules = new Map([
        ['gi://Clutter', {default: {Clone: Actor}}], ['gi://Gio', {default: Gio}],
        ['gi://GLib', {default: GLib}], ['gi://Meta', {default: Meta}],
        ['resource:///org/gnome/shell/extensions/extension.js', {Extension}],
        ['resource:///org/gnome/shell/ui/main.js', {layoutManager, overview, sessionMode}],
        ['./overview.mjs', {OverviewClones}],
    ]);
    const module = new vm.SourceTextModule(extensionSource, {context});
    await module.link(specifier => {
        if (specifier === './protocol.mjs')
            return new vm.SourceTextModule(protocolSource, {context});
        const exports = modules.get(specifier);
        assert.ok(exports, `unexpected dependency ${specifier}`);
        return new vm.SyntheticModule(Object.keys(exports), function () {
            for (const [name, value] of Object.entries(exports))
                this.setExport(name, value);
        }, {context});
    });
    await module.evaluate();
    h.extension = new module.namespace.default();
    h.enable = () => {
        h.extension.enable();
        [...busOwners.values()].at(-1).acquired();
    };
    h.enable();
    h.call = (method, params = [], sender = ':1.42') => {
        let reply;
        h.extension[`${method}Async`](params, {
            get_sender: () => sender,
            return_value: variant => { reply = {signature: variant.signature, value: Array.from(variant.value)}; },
            return_dbus_error: (name, message) => { reply = {error: name, message}; },
        });
        assert.ok(reply, 'D-Bus invocation must always receive a reply');
        return reply;
    };
    h.start = (params = request, owner) => {
        const reply = h.call('Start', params, owner);
        assert.equal(reply.error, undefined, reply.message);
        assert.match(reply.value[0], /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$/);
        return reply.value[0];
    };
    h.window = (token, properties = {}) => {
        const window = Object.assign(new Window(), {
            title: `wallbreak-renderer:${token}`, appId: APP_ID, ...properties,
        });
        window.sourceParent = new Actor();
        window.sourceParent.add_child(window.actor);
        display.emit('window-created', window);
        return window;
    };
    h.advance = async ms => {
        const target = now + ms;
        while (true) {
            const next = [...timers.entries()].filter(([, timer]) => timer.at <= target)
                .sort((a, b) => a[1].at - b[1].at)[0];
            if (!next)
                break;
            now = next[1].at;
            timers.delete(next[0]);
            next[1].callback();
            await flush();
        }
        now = target;
        await flush();
    };
    h.loseOwner = (owner = ':1.42') => {
        for (const watch of [...watches.values()]) {
            if (watch.name === owner)
                watch.vanished();
        }
    };
    h.status = (token, owner) => h.call('Status', [token], owner);
    h.process = () => spawned.at(-1).process;
    return h;
}

test('activation uses GNOME 50 session information without a removed Meta API', async () => {
    const h = await harness();
    assert.deepEqual(h.call('Info').value, [1]);
    for (const sessionType of ['x11', '', null])
        await assert.rejects(harness({sessionType}), /requires GNOME 50 on Wayland/);
});

test('metadata and canonical D-Bus contract are GNOME 50/protocol 1 only', async () => {
    assert.equal(metadata.uuid, 'wallbreak@flakesonnix.github.io');
    assert.deepEqual(metadata['shell-version'], ['50']);
    assert.match(xml, new RegExp(`interface name="${INTERFACE.replaceAll('.', '\\.')}"`));
    const signatures = {};
    for (const match of xml.matchAll(/<method name="(\w+)">([\s\S]*?)<\/method>/g)) {
        const args = [...match[2].matchAll(/<arg name="([^"]+)" type="([^"]+)" direction="([^"]+)"\/>/g)];
        signatures[match[1]] = {
            in: args.filter(arg => arg[3] === 'in').map(arg => arg[2]).join(''),
            out: args.filter(arg => arg[3] === 'out').map(arg => arg[2]).join(''),
        };
    }
    assert.deepEqual(signatures, {
        Info: {in: '', out: 'u'}, Start: {in: 'ssusss', out: 's'},
        Status: {in: 's', out: 'ss'}, Stop: {in: 's', out: ''},
    });
    const h = await harness();
    assert.equal(h.exportedPath, OBJECT_PATH);
    assert.deepEqual(h.call('Info'), {signature: '(u)', value: [1]});
});

test('request validation preserves paths as data and enforces typed resource limits', () => {
    const parsed = validateRequest(request);
    const geometry = frameGeometry({width: 1920, height: 1080}, 2);
    assert.deepEqual(rendererArgs(parsed, 'nonce', geometry), [
        request[0], '--source', request[1], '--session-token', 'nonce',
        '--width', '3840', '--height', '2160', '--logical-width', '1920',
        '--logical-height', '1080', '--fps', '30', '--fit', 'cover', '--ffmpeg', 'ffmpeg',
    ]);
    for (const [index, values] of [
        [0, ['wallbreak-renderer', '/tmp/other', '/tmp/wallbreak-renderer/', '/tmp/\0/wallbreak-renderer']],
        [1, ['relative.mp4', '', '/bad\0path']], [2, [0, 61, 1.5, '30', NaN]],
        [3, ['stretch', '']], [4, ['', '\0']], [5, [null, '\0']],
    ]) {
        for (const value of values) {
            const invalid = [...request];
            invalid[index] = value;
            assert.throws(() => validateRequest(invalid), {code: 'InvalidArgument'});
        }
    }
    assert.deepEqual(frameGeometry({width: 4096, height: 4096}, 1), {
        width: 4096, height: 4096, logicalWidth: 4096, logicalHeight: 4096,
    });
    assert.equal(4096 * 4096 * 4, MAX_FRAME_BYTES);
    for (const [monitor, scale] of [
        [{width: 4097, height: 4096}, 1], [{width: 0, height: 1}, 1],
        [{width: 1, height: 1}, 1.5], [{width: 1920, height: 1080}, 3],
        [{width: Number.MAX_SAFE_INTEGER, height: 1}, 2], [{width: 1, height: 1}, Infinity],
    ])
        assert.throws(() => frameGeometry(monitor, scale), {code: 'InvalidArgument'});
});

test('all ownership factors are mandatory, independently', () => {
    const identity = {native: true, owned: true, pid: 700, expectedPid: 700,
        title: 'wallbreak-renderer:nonce', token: 'nonce', appId: APP_ID};
    assert.equal(matchesRenderer(identity), true);
    for (const change of [{native: false}, {owned: false}, {pid: 701}, {expectedPid: 0},
        {title: 'wallbreak-renderer:other'}, {appId: 'other'}, {token: 'other'}])
        assert.equal(matchesRenderer({...identity, ...change}), false);
});

test('stderr retains a bounded byte tail, labels truncation, and tolerates split UTF-8/NUL', () => {
    const tail = new StderrTail();
    tail.append(new TextEncoder().encode('a'.repeat(MAX_STDERR_BYTES)));
    assert.equal(tail.truncated, false);
    tail.append(new TextEncoder().encode('last\0\u00e9'));
    assert.equal(tail.bytes.length, MAX_STDERR_BYTES);
    assert.match(tail.text(), /^\[stderr truncated; last 32768 bytes\]/);
    assert.ok(tail.text().endsWith('last\\0\u00e9'));
    tail.append(new Uint8Array(MAX_STDERR_BYTES * 4).fill(120));
    assert.equal(tail.bytes.length, MAX_STDERR_BYTES);
    assert.ok(tail.text().endsWith('x'.repeat(MAX_STDERR_BYTES)));
    const utf8 = new StderrTail();
    utf8.append(Uint8Array.of(0xc3));
    utf8.append(Uint8Array.of(0xa9));
    assert.equal(utf8.text(), '\u00e9');
});

test('Start validates file, connector and geometry before spawning; never replaces a session', async () => {
    const h = await harness();
    for (const field of ['fileError', 'directory', 'notExecutable']) {
        h[field] = true;
        assert.ok(h.call('Start', request).error);
        h[field] = false;
    }
    assert.match(h.call('Start', [...request.slice(0, 5), 'DP-9']).error, /InvalidArgument$/);
    h.scale = 4;
    assert.match(h.call('Start', request).error, /InvalidArgument$/);
    assert.equal(h.spawned.length, 0);
    h.scale = 2;
    const token = h.start([...request.slice(0, 5), 'DP-1']);
    assert.equal(h.queriedPath, request[0]);
    assert.equal(h.launcherClosed, true);
    assert.deepEqual(h.spawned[0].argv, rendererArgs(validateRequest(request), token,
        frameGeometry(h.layoutManager.monitors[0], 2)));
    assert.match(h.call('Start', request, ':1.43').error, /Busy$/);
    assert.match(h.status(token, ':1.43').error, /NotOwner$/);
    assert.match(h.call('Stop', [token], ':1.43').error, /NotOwner$/);
    assert.match(h.status('unknown').error, /UnknownSession$/);
    assert.equal(h.status(token).value[0], 'starting');
    assert.deepEqual(h.process().sent, []);
});

test('overview transitions never hide or replace the desktop clone before its native parent', async () => {
    const h = await harness();
    const token = h.start();
    const window = h.window(token);
    window.shown();
    const background = h.manager.backgroundActor;
    const desktopGroup = new Actor();
    desktopGroup.add_child(background);
    const clone = background.children[0];
    const owner = h.overviewOwners[0];
    // Include reversal mid-transition, not just the settled overview states.
    for (const signal of ['showing', 'shown', 'hiding', 'showing', 'hiding', 'hidden']) {
        desktopGroup.visible = true;
        h.overview.emit(signal);
        assert.equal(clone.visible, true, `desktop still contributes during ${signal}`);
        assert.equal(background.children[0], clone);
        assert.equal(clone.source, window.actor);
        assert.equal(owner.allowed, true);
        assert.equal(owner.overviewVisible, signal !== 'hidden');
        assert.equal(desktopGroup.visible, true, 'only native Shell code hides the parent');
        desktopGroup.visible = false;
        h.sessionMode.emit('updated');
        assert.equal(clone.visible, true, 'retain the clone under its hidden native parent');
        assert.equal(desktopGroup.visible, false);
        assert.equal(window.actor.opacity, 0);
        assert.equal(window.inhibitors, 1);
    }
    h.call('Stop', [token]);
    h.process().exit();
    await flush();
    assert.equal(clone.destroyed, true);
    assert.equal(h.status(token).value[0], 'stopped');
});

test('delayed metadata, clone reattachment and overview visibility keep the same privacy gate', async () => {
    const h = await harness();
    const token = h.start();
    const window = h.window(token, {title: null, appId: null});
    assert.equal(window.actor.opacity, 255);
    window.title = `wallbreak-renderer:${token}`;
    window.emit('notify::title');
    assert.deepEqual(window.touched, []);
    window.appId = APP_ID;
    window.emit('notify::wm-class');
    assert.deepEqual(window.touched, [['type', 4], ['hide']]);
    assert.equal(window.actor.opacity, 0);
    assert.equal(window.inhibitors, 0);
    assert.equal(h.manager.backgroundActor.children.length, 0);
    window.shown();
    window.shown();
    assert.equal(window.inhibitors, 1);
    assert.equal(h.status(token).value[0], 'running');
    let clone = h.manager.backgroundActor.children[0];
    assert.equal(clone.source, window.actor);
    assert.equal(clone.reactive, false);
    assert.equal(window.actor.parent, window.sourceParent);
    assert.equal(clone.visible, true);
    const overview = h.overviewOwners[0];
    assert.equal(overview.source, window.actor);
    assert.equal(overview.monitor, h.layoutManager.monitors[0]);
    assert.equal(overview.index, 0);
    assert.equal(overview.allowed, true);
    assert.equal(overview.overviewVisible, false);
    h.overview.emit('showing');
    assert.equal(clone.visible, true);
    assert.equal(overview.allowed, true);
    assert.equal(overview.overviewVisible, true);
    h.overview.emit('hidden');
    assert.equal(clone.visible, true);
    assert.equal(overview.overviewVisible, false);
    h.sessionMode.isLocked = true;
    h.sessionMode.emit('updated');
    assert.equal(clone.visible, false);
    assert.equal(overview.allowed, false);
    h.sessionMode.isLocked = false;
    h.sessionMode.emit('updated');
    h.layoutManager.screenShieldGroup.visible = true;
    h.layoutManager.screenShieldGroup.emit('notify::visible');
    assert.equal(clone.visible, false);
    assert.equal(overview.allowed, false);
    h.layoutManager.screenShieldGroup.visible = false;
    h.layoutManager.screenShieldGroup.emit('notify::visible');
    assert.equal(clone.visible, true);
    assert.equal(overview.allowed, true);
    const old = h.manager.backgroundActor;
    h.manager.backgroundActor = new Actor();
    h.manager.emit('changed');
    assert.equal(clone.destroyed, true);
    assert.equal(old.children.length, 0);
    clone = h.manager.backgroundActor.children[0];
    assert.equal(clone.source, window.actor);
    assert.equal(clone.visible, true);
    h.call('Stop', [token]);
    h.process().exit(0, {signal: 15});
    await flush();
    assert.equal(window.inhibitors, 0);
    assert.equal(window.actor.opacity, 0, 'do not flash the source during shutdown');
    assert.equal(window.actor.parent, window.sourceParent);
    assert.equal(clone.destroyed, true);
    assert.equal(overview.destroyed, true);
    assert.equal(overview.hasResources, false);
    assert.equal(overview.source, null);
    assert.equal(h.status(token).value[0], 'stopped');
    assert.equal(h.timers.size, 0);
    assert.equal(h.display.handlers.size, 0);
    assert.equal(window.handlers.size, 0);
});

test('playback started in overview shows there without changing desktop focus or lock visibility', async () => {
    const h = await harness();
    h.overview.visible = true;
    const token = h.start();
    const window = h.window(token);
    window.shown();
    assert.equal(h.status(token).value[0], 'running');
    const clone = h.manager.backgroundActor.children[0];
    assert.equal(clone.visible, true, 'native desktop ancestry controls overview occlusion');
    const owner = h.overviewOwners[0];
    assert.equal(owner.allowed, true);
    assert.equal(owner.overviewVisible, true);
    for (const [field, value] of [['isLocked', true], ['isGreeter', true], ['hasWindows', false]]) {
        const previous = h.sessionMode[field];
        h.sessionMode[field] = value;
        h.sessionMode.emit('updated');
        assert.equal(owner.allowed, false, field);
        assert.equal(clone.visible, false, field);
        h.sessionMode[field] = previous;
        h.sessionMode.emit('updated');
        assert.equal(owner.allowed, true, field);
        assert.equal(clone.visible, true, field);
    }
    h.layoutManager.screenShieldGroup.visible = true;
    h.layoutManager.screenShieldGroup.emit('notify::visible');
    assert.equal(owner.allowed, false);
    assert.equal(clone.visible, false);
    h.call('Stop', [token]);
    h.process().exit();
    await flush();
    assert.equal(owner.destroyed, true);
});

test('overview startup failure cannot resurrect a stopping session as running', async () => {
    const h = await harness();
    h.overviewStartError = true;
    const token = h.start();
    h.window(token).shown();
    assert.equal(h.status(token).value[0], 'stopping');
    assert.deepEqual(h.process().sent, [15]);
    assert.equal(h.overviewOwners[0].destroyed, true);
    h.process().exit();
    await flush();
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /Overview integration failed/);
});

test('failed overview cleanup retains the session until its resources are released', async () => {
    const h = await harness();
    const token = h.start();
    h.window(token).shown();
    const owner = h.overviewOwners[0];
    h.overviewCleanupError = true;
    h.call('Stop', [token]);
    h.process().exit();
    await flush();
    assert.equal(h.extension._session.overviewClones, owner);
    assert.equal(owner.hasResources, true);
    assert.equal(h.extension._session.settled, false);
    assert.match(h.call('Start', request).error, /Busy$/);
    h.overviewCleanupError = false;
    await h.advance(1000);
    assert.equal(owner.hasResources, false);
    assert.equal(h.extension._session.overviewClones, null);
    assert.equal(h.extension._session.settled, true);
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /Overview cleanup failed/);
});

test('spoofed/foreign windows are never touched and readiness timeout tears down only the child', async () => {
    const h = await harness();
    const token = h.start();
    const windows = [{owned: false}, {native: false}, {pid: 701},
        {title: 'wallbreak-renderer:other'}, {appId: 'other'}].map(properties => h.window(token, properties));
    for (const window of windows) {
        window.shown();
        assert.deepEqual(window.touched, []);
        assert.equal(window.actor.opacity, 255);
        assert.equal(window.inhibitors, 0);
    }
    await h.advance(20_000);
    assert.equal(h.status(token).value[0], 'stopping');
    assert.deepEqual(h.process().sent, [15]);
    h.process().exit(1);
    await flush();
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /readiness timed out/);
});

test('Stop is asynchronous/idempotent, blocks Start until reaped, and escalates explicitly', async () => {
    const h = await harness();
    const token = h.start();
    const window = h.window(token);
    window.shown();
    assert.deepEqual(h.call('Stop', [token]), {signature: '()', value: []});
    h.call('Stop', [token]);
    assert.deepEqual(h.process().sent, [15]);
    assert.equal(h.status(token).value[0], 'stopping');
    assert.match(h.call('Start', request).error, /Busy$/);
    assert.equal(window.inhibitors, 0);
    assert.equal(h.manager.backgroundActor.children.length, 0);
    await h.advance(2999);
    assert.deepEqual(h.process().sent, [15]);
    await h.advance(1);
    assert.deepEqual(h.process().sent, [15, 9]);
    assert.match(h.status(token).value[1], /Forced cleanup/);
    h.process().exit(0, {signal: 9});
    await flush();
    assert.equal(h.status(token).value[0], 'failed');
    h.call('Stop', [token]);
    assert.equal(h.status(token).value[0], 'failed', 'forced decoder cleanup stays uncertain');
    assert.match(h.status(token).value[1], /Forced cleanup/);
    const next = h.start();
    assert.notEqual(next, token);
    assert.match(h.status(token).error, /UnknownSession$/);
});

test('renderer failure retains bounded stderr only for the owning session', async () => {
    const h = await harness();
    const token = h.start();
    h.process().stream.feed(new TextEncoder().encode('x'.repeat(40_000) + '\nfatal decoder error'));
    h.process().exit(2);
    await flush();
    const reply = h.status(token);
    assert.equal(reply.value[0], 'failed');
    assert.match(reply.value[1], /status 2/);
    assert.match(reply.value[1], /stderr truncated/);
    assert.ok(reply.value[1].endsWith('fatal decoder error'));
    assert.match(h.status(token, ':1.99').error, /NotOwner$/);
    assert.equal(h.extension._session.tail.bytes.length, MAX_STDERR_BYTES);
    assert.equal(h.extension._session.client, null);
    assert.equal(h.extension._session.process, null);
    h.call('Stop', [token]);
    assert.equal(h.status(token).value[0], 'failed', 'a recorded decoder failure stays failed after Stop');
    assert.ok(h.status(token).value[1].endsWith('fatal decoder error'));
    h.loseOwner();
    assert.match(h.status(token).error, /UnknownSession$/);
    assert.equal(h.watches.size, 0);
});

test('spawn failures return owned diagnostics without a phantom active process', async () => {
    const h = await harness();
    h.spawnError = true;
    const token = h.start();
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /spawn denied/);
    assert.equal(h.spawned.length, 0);
    assert.equal(h.timers.size, 0);
    assert.equal(h.display.handlers.size, 0);
    h.spawnError = false;
    h.start();
    assert.equal(h.spawned.length, 1);
});

test('caller loss removes visuals, terminates/reaps child and forgets final diagnostics', async () => {
    const h = await harness();
    const token = h.start();
    const window = h.window(token);
    window.shown();
    h.loseOwner();
    assert.equal(window.inhibitors, 0);
    assert.equal(h.manager.backgroundActor.children.length, 0);
    assert.deepEqual(h.process().sent, [15]);
    assert.match(h.call('Start', request, ':1.43').error, /Busy$/);
    assert.match(h.status(token).error, /UnknownSession$/);
    h.process().exit();
    await flush();
    assert.equal(h.extension._session, null);
    assert.equal(h.timers.size, 0);
});

test('monitor changes terminate safely; an index is never reinterpreted as another output', async () => {
    const h = await harness();
    const token = h.start([...request.slice(0, 5), 'DP-1']);
    h.window(token).shown();
    h.layoutManager.monitors = [{width: 800, height: 600}];
    h.layoutManager.emit('monitors-changed');
    assert.deepEqual(h.process().sent, [15]);
    assert.equal(h.manager.backgroundActor.children.length, 0);
    h.process().exit();
    await flush();
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /Monitor configuration changed/);
});

test('disable closes export/name without dropping ownership, including immediate reenable', async () => {
    const h = await harness();
    const token = h.start();
    const window = h.window(token);
    window.shown();
    const oldBus = [...h.busOwners.values()][0];
    const oldExport = h.exported;
    const retained = h.extension._session;
    h.extension.disable();
    h.extension.disable();
    assert.equal(oldExport.closed, true);
    assert.equal(h.busOwners.size, 0);
    assert.equal(h.watches.size, 0);
    assert.equal(window.inhibitors, 0);
    assert.equal(retained.client, h.spawned[0].client);
    assert.deepEqual(h.process().sent, [15]);
    h.enable();
    oldBus.lost();
    assert.notEqual(h.exported.closed, true, 'stale name callbacks must not close a newer export');
    assert.match(h.call('Start', request).error, /Busy$/);
    h.process().exit();
    await flush();
    assert.equal(retained.process, null);
    assert.equal(h.extension._session, null);
    assert.equal(h.timers.size, 0);
    h.start();
});

test('failed waits retain the child and retry instead of assuming it exited', async () => {
    const h = await harness();
    const token = h.start();
    const process = h.process();
    process.wait(process, new Error('temporary wait error'));
    assert.equal(h.status(token).value[0], 'stopping');
    assert.deepEqual(process.sent, [15]);
    assert.equal(h.extension._session.process, process);
    assert.match(h.call('Start', request).error, /Busy$/);
    await h.advance(1000);
    process.exit(1);
    await flush();
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /temporary wait error/);
});

test('inherited stderr pipe cannot keep a reaped session busy indefinitely', async () => {
    const h = await harness();
    const token = h.start();
    h.call('Stop', [token]);
    h.process().exit(0, {eof: false});
    await flush();
    assert.equal(h.status(token).value[0], 'stopping');
    await h.advance(1000);
    assert.equal(h.status(token).value[0], 'stopped');
    assert.match(h.status(token).value[1], /Stderr drain truncated/);
    assert.equal(h.timers.size, 0);
    h.start();
});

test('lost bus name and changed owned-window identity fail closed', async () => {
    for (const cause of ['bus', 'identity', 'unmanaged']) {
        const h = await harness();
        const token = h.start();
        const window = h.window(token);
        window.shown();
        if (cause === 'bus')
            [...h.busOwners.values()][0].lost();
        else if (cause === 'identity') {
            window.title = 'changed';
            window.emit('notify::title');
        } else {
            window.emit('unmanaged');
        }
        assert.deepEqual(h.process().sent, [15]);
        assert.equal(window.inhibitors, 0);
        h.process().exit();
        await flush();
        assert.equal(h.status(token).value[0], 'failed');
        assert.equal(h.timers.size, 0);
    }
});

test('synchronous wait/read scheduling failures keep ownership and can finish cleanup', async () => {
    for (const failure of ['waitScheduleError', 'readScheduleError']) {
        const h = await harness();
        h[failure] = true;
        const token = h.start();
        const process = h.process();
        assert.equal(h.status(token).value[0], 'stopping');
        assert.deepEqual(process.sent, [15]);
        assert.equal(h.extension._session.process, process);
        assert.match(h.call('Start', request).error, /Busy$/);
        process.scheduleError = false;
        process.stream.scheduleError = false;
        if (failure === 'waitScheduleError')
            await h.advance(1000);
        process.exit(1);
        await flush();
        assert.equal(h.status(token).value[0], 'failed');
        assert.equal(h.timers.size, 0);
        h.start();
    }
});

test('stderr read and close failures remain visible without discarding unfinished resources', async () => {
    const h = await harness();
    const token = h.start();
    const process = h.process();
    process.stream.closeError = true;
    process.stream.deliver(new Error('read failed'));
    await flush();
    assert.deepEqual(process.sent, [15]);
    assert.equal(h.status(token).value[0], 'stopping');
    process.exit(1);
    await flush();
    assert.equal(h.extension._session.process, process);
    assert.equal(h.extension._session.stderr, process.stream);
    assert.match(h.call('Start', request).error, /Busy$/);
    process.stream.closeError = false;
    await h.advance(1000);
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /read failed/);
    assert.match(h.status(token).value[1], /close failed/);
    assert.equal(h.timers.size, 0);
});

test('failed mapping cleanup retains the owner and retries after reaping', async () => {
    const h = await harness();
    const token = h.start();
    const window = h.window(token);
    window.shown();
    const uninhibit = window.uninhibit_mapped;
    window.uninhibit_mapped = () => { throw new Error('temporary mapping failure'); };
    h.call('Stop', [token]);
    h.process().exit();
    await flush();
    assert.equal(window.inhibitors, 1);
    assert.equal(h.extension._session.window, window);
    assert.equal(h.extension._session.settled, false);
    assert.match(h.call('Start', request).error, /Busy$/);
    window.uninhibit_mapped = uninhibit;
    await h.advance(1000);
    assert.equal(window.inhibitors, 0);
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /temporary mapping failure/);
    assert.equal(h.timers.size, 0);
});

test('stale signals/timers cannot affect a newer session after complete cleanup', async () => {
    const h = await harness();
    const first = h.start();
    const staleTimer = [...h.timers.values()][0].callback;
    const staleWindowCreated = [...h.display.handlers.values()][0].callback;
    const staleOwnerLost = [...h.watches.values()][0].vanished;
    const window = h.window(first);
    window.shown();
    h.call('Stop', [first]);
    h.process().exit();
    await flush();
    const second = h.start();
    staleTimer();
    staleWindowCreated(h.display, window);
    staleOwnerLost();
    assert.equal(h.status(second).value[0], 'starting');
    assert.deepEqual(h.process().sent, []);
    assert.equal(h.manager.backgroundActor.children.length, 0);
});

test('a crash during graceful shutdown is failed, not a successful Stop', async () => {
    for (const exit of [{code: 2, signal: 0}, {code: 0, signal: 11}]) {
        const h = await harness();
        const token = h.start();
        h.call('Stop', [token]);
        h.process().exit(exit.code, {signal: exit.signal});
        await flush();
        assert.equal(h.status(token).value[0], 'failed');
        assert.match(h.status(token).value[1], exit.signal ? /signal 11/ : /status 2/);
    }
});

test('identity arriving after first shown fails rather than retrofitting a foreground window', async () => {
    const h = await harness();
    const token = h.start();
    const window = h.window(token, {title: null});
    window.shown();
    window.title = `wallbreak-renderer:${token}`;
    window.emit('notify::title');
    assert.deepEqual(window.touched, []);
    assert.equal(window.actor.opacity, 255);
    assert.deepEqual(h.process().sent, [15]);
    h.process().exit();
    await flush();
    assert.equal(h.status(token).value[0], 'failed');
    assert.match(h.status(token).value[1], /identity was not ready before first shown/);
});
