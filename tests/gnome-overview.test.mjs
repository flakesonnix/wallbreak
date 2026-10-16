// Run: node --experimental-vm-modules --test tests/gnome-overview.test.mjs
// Simulated Shell/GI boundaries, not live overview or pixel verification.
// API basis: GNOME Shell 50.2 workspace.js/background.js and Meta/Clutter 18 GIR;
// read-only GJS reflection confirmed Shell.GLSLEffect.set_uniform_float(loc, n, values),
// connect_after, parent-set (not notify::parent), BindCoordinate.SIZE and no bounds getter.
// layout.js leaves overviewGroup's St.Widget layout unspecified; its reflected
// class layout type is null, selecting ClutterActor's FixedLayout fallback.
// Cogl 18 documents cogl_tex_coord0_in/cogl_color_out in fragment snippets;
// queue_repaint is inherited from Clutter.Effect, not invented by this fixture.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const source = await readFile(new URL(
    '../extensions/wallbreak@flakesonnix.github.io/overview.mjs', import.meta.url), 'utf8');

class Signals {
    handlers = new Map();
    next = 0;
    connect(name, callback) { return this._connect(name, callback, false); }
    connect_after(name, callback) { return this._connect(name, callback, true); }
    _connect(name, callback, after) {
        if (this.connectError)
            throw new Error('connect failed');
        const id = ++this.next;
        this.handlers.set(id, {name, callback, after});
        return id;
    }
    disconnect(id) {
        if (this.disconnectError)
            throw new Error('disconnect failed');
        assert.ok(this.handlers.delete(id), `unknown signal ${id}`);
    }
    emit(name, ...args) {
        const handlers = [...this.handlers.entries()].sort((a, b) => a[1].after - b[1].after);
        for (const [id, signal] of handlers) {
            if (this.handlers.has(id) && signal.name === name)
                signal.callback(this, ...args);
        }
    }
}

// Model the verified FixedLayout policy, not a native compositor execution:
// fixed child origins and explicit size requests are independent of the parent.
class FixedLayout {
    allocate(container) {
        for (const child of container.children) {
            const [x, y] = child.fixedPosition ?? [child.x, child.y];
            const [width, height] = child.sizeRequest ?? [child.width, child.height];
            child.allocate(x, y, width, height);
        }
    }
}

class Actor extends Signals {
    children = [];
    parent = null;
    constraints = [];
    effects = [];
    visible = true;
    reactive = true;
    x = 0;
    y = 0;
    width = 0;
    height = 0;
    constructor(properties = {}) {
        super();
        Object.assign(this, properties);
    }
    get_parent() { return this.parent; }
    get_children() { return [...this.children]; }
    add_child(child) { this.insert_child_at_index(child, this.children.length); }
    insert_child_at_index(child, index) {
        assert.equal(child.parent, null, 'never reparent an existing actor');
        this.children.splice(index, 0, child);
        child.parent = this;
        child.emit('parent-set', null);
    }
    remove_child(child) {
        assert.equal(child.parent, this);
        this.children.splice(this.children.indexOf(child), 1);
        child.parent = null;
        child.emit('parent-set', this);
    }
    set_position(x, y) {
        this.fixedPosition = [x, y];
        Object.assign(this, {x, y});
    }
    set_size(width, height) {
        this.sizeRequest = [width, height];
        Object.assign(this, {width, height});
    }
    has_allocation() { return this.allocated === true; }
    allocate(x, y, width, height) {
        Object.assign(this, {x, y, width, height, allocated: true});
        this.allocation = {x, y, width, height};
        for (const child of this.children) {
            const constraint = child.constraints.find(item => item.source === this);
            if (constraint) {
                assert.equal(constraint.coordinate, 5, 'SIZE only: no second monitor offset');
                child.set_size(width, height);
                child.allocated = true;
            }
        }
        this.layout_manager?.allocate(this);
        this.emit('notify::allocation');
    }
    add_constraint(constraint) { this.constraints.push(constraint); }
    remove_constraint(constraint) {
        if (this.constraintError)
            throw new Error('constraint removal failed');
        const index = this.constraints.indexOf(constraint);
        assert.notEqual(index, -1);
        this.constraints.splice(index, 1);
    }
    add_effect(effect) { this.effects.push(effect); }
    remove_effect(effect) {
        if (this.effectError)
            throw new Error('effect removal failed');
        const index = this.effects.indexOf(effect);
        assert.notEqual(index, -1);
        this.effects.splice(index, 1);
    }
    destroy() {
        if (this.destroyError)
            throw new Error('actor destruction failed');
        assert.ok(!this.destroyed, 'do not destroy an actor twice');
        this.destroyed = true;
        // Clutter's destroy signal is RUN_CLEANUP; children may still be present.
        this.emit('destroy');
        for (const child of [...this.children])
            child.destroy();
        if (this.parent)
            this.parent.remove_child(this);
        this.handlers.clear();
        this.constraints = [];
        this.effects = [];
        this.source = null;
    }
}

async function harness({open = false, beforeHook = null, onError = null,
    monitorGeometry = {x: -1920, y: 180, width: 1920, height: 1080}} = {}) {
    const actors = [];
    const clones = [];
    const effects = [];
    const errors = [];
    const idles = new Map();
    const calls = [];
    const display = new Signals();
    const theme = Object.assign(new Signals(), {scaleFactor: 2});
    const overviewGroup = new Actor({visible: open, layout_manager: new FixedLayout()});
    const controls = new Actor({name: 'native-controls'});
    overviewGroup.add_child(controls);
    const monitor = {...monitorGeometry};
    const layoutManager = Object.assign(new Signals(), {
        monitors: [monitor, {x: 0, y: 0, width: 2560, height: 1440}], overviewGroup,
    });
    const sourceParent = new Actor({visible: false});
    const sourceActor = new Actor({opacity: 0});
    sourceParent.add_child(sourceActor);
    let nextIdle = 0;
    const h = {actors, clones, effects, errors, idles, calls, display, theme, monitor,
        layoutManager, overviewGroup, controls, sourceActor, sourceParent};
    class Clone extends Actor {
        constructor(properties) {
            if (h.cloneError)
                throw new Error('clone creation failed');
            super(properties);
            clones.push(this);
        }
    }
    class BackgroundActor extends Actor {
        constructor(index) {
            super({monitor: index, content: {rounded_clip_radius: 0,
                vignette: false, brightness: 0.5, nativeBackground: {}}});
            actors.push(this);
        }
    }
    // Models the producing boundary, including factory-before-field assignment.
    // It is not GNOME's implementation and intentionally has no settings/window API.
    class BackgroundManager {
        constructor(container, index = 0) {
            Object.assign(this, {_container: container, _monitorIndex: index,
                _layoutManager: layoutManager, _settingsSchema: 'org.gnome.desktop.background'});
            this.backgroundActor = this._createBackgroundActor('construct', {index});
            this._newBackgroundActor = null;
        }
        _createBackgroundActor(...args) {
            calls.push({manager: this, args});
            if (h.nativeError)
                throw new Error('native factory failed');
            const actor = new BackgroundActor(this._monitorIndex);
            this._container.add_child(actor);
            return actor;
        }
        pending() {
            this._newBackgroundActor?.destroy();
            this._newBackgroundActor = this._createBackgroundActor('pending');
            return this._newBackgroundActor;
        }
        swap(immediate = false) {
            const old = this.backgroundActor;
            this.backgroundActor = this._newBackgroundActor;
            this._newBackgroundActor = null;
            if (immediate)
                old.destroy();
            else
                old.fading = true;
            return old;
        }
    }
    class WorkspaceBackground extends Actor {
        constructor({index = 0, parent = controls, incomplete = false} = {}) {
            super();
            this._monitorIndex = index;
            this._workarea = {x: monitor.x + 16, y: monitor.y + 32, width: 1880, height: 1020};
            this._stateAdjustment = {value: 0.75};
            this._bin = new Actor({clip_to_allocation: true});
            this._backgroundGroup = new Actor();
            this._bin.add_child(this._backgroundGroup);
            this.add_child(this._bin);
            // Native binding exists before the background manager is constructed.
            this.state_adjustment_value = 0.75;
            const manager = new BackgroundManager(this._backgroundGroup, index);
            if (!incomplete)
                this._bgManager = manager;
            if (parent)
                parent.add_child(this);
            this.manager = manager;
            this.nativeAllocate();
        }
        nativeAllocate() {
            for (const actor of this._backgroundGroup.children)
                actor.allocate(-10, -20, 960, 540);
            this.emit('notify::allocation');
        }
        state(value) {
            this._stateAdjustment.value = value;
            this.state_adjustment_value = value;
            this.emit('notify::state-adjustment-value');
        }
    }
    class GLSLEffect {
        constructor() { this._init(); }
        _init() {
            if (h.maskError)
                throw new Error('mask creation failed');
            this.uniforms = new Map();
            this.vfunc_build_pipeline();
            effects.push(this);
        }
        add_glsl_snippet(hook, declarations, code, replace) {
            assert.equal(hook, 2048);
            assert.equal(replace, false);
            this.snippet = {declarations, code};
        }
        get_uniform_location(name) { return name; }
        set_uniform_float(location, components, values) {
            assert.equal(arguments.length, 3, 'GI hides the array-length argument');
            assert.equal(values.length, components);
            if (h.uniformError)
                throw new Error('uniform update failed');
            this.uniforms.set(location, Array.from(values));
        }
        queue_repaint() { this.repaints = (this.repaints ?? 0) + 1; }
    }
    Object.assign(h, {BackgroundManager, BackgroundActor, WorkspaceBackground});
    const original = Object.getOwnPropertyDescriptor(BackgroundManager.prototype, '_createBackgroundActor');
    if (beforeHook)
        beforeHook(BackgroundManager.prototype, original.value);
    h.originalDescriptor = Object.getOwnPropertyDescriptor(BackgroundManager.prototype, '_createBackgroundActor');
    const warnings = [];
    h.warnings = warnings;
    const context = vm.createContext({global: {display, stage: {}},
        console: {error: message => warnings.push(message)}});
    const modules = new Map([
        ['gi://Clutter', {default: {Clone, BindConstraint: class {
            constructor(properties) { Object.assign(this, properties); }
        }, BindCoordinate: {SIZE: 5}}}],
        ['gi://Cogl', {default: {SnippetHook: {FRAGMENT: 2048}}}],
        ['gi://GObject', {default: {registerClass: type => type}}],
        ['gi://Meta', {default: {BackgroundActor}}],
        ['gi://Shell', {default: {GLSLEffect}}],
        ['gi://St', {default: {ThemeContext: {get_for_stage: () => theme}}}],
        ['gi://GLib', {default: {PRIORITY_DEFAULT_IDLE: 200, SOURCE_REMOVE: false,
            idle_add: (priority, callback) => {
                assert.equal(priority, 200);
                if (h.idleAddError)
                    throw new Error('idle scheduling failed');
                const id = ++nextIdle;
                idles.set(id, callback);
                return id;
            },
            source_remove: id => {
                if (h.idleRemoveError)
                    throw new Error('idle removal failed');
                assert.ok(idles.delete(id), 'only remove a live owned source');
                return true;
            }}}],
        ['resource:///org/gnome/shell/ui/background.js', {BackgroundManager}],
        ['resource:///org/gnome/shell/ui/main.js', {layoutManager}],
        ['resource:///org/gnome/shell/ui/workspace.js', {WorkspaceBackground}],
    ]);
    const module = new vm.SourceTextModule(source, {context});
    await module.link(specifier => {
        const exports = modules.get(specifier);
        assert.ok(exports, `unexpected dependency ${specifier}`);
        return new vm.SyntheticModule(Object.keys(exports), function () {
            for (const [name, value] of Object.entries(exports))
                this.setExport(name, value);
        }, {context});
    });
    await module.evaluate();
    h.owner = new module.namespace.OverviewClones(sourceActor, monitor, 0, error => {
        errors.push(error);
        onError?.(h, error);
    });
    h.flush = () => {
        // Exactly one bounded idle turn, not a polling loop.
        for (const [id, callback] of [...idles]) {
            if (idles.delete(id))
                assert.equal(callback(), false);
        }
    };
    h.backdrop = () => clones.find(clone => clone.parent === overviewGroup);
    h.card = background => background.children.find(child => child instanceof Clone);
    h.assertClean = () => {
        assert.equal(h.owner.hasResources, false);
        assert.equal(idles.size, 0);
        assert.equal(theme.handlers.size, 0);
        assert.equal(display.handlers.size, 0);
        assert.equal(layoutManager.handlers.size, 0);
        assert.equal(sourceActor.handlers.size, 0);
        for (const clone of clones) {
            assert.ok(clone.destroyed);
            assert.equal(clone.handlers.size, 0);
            assert.equal(clone.source, null);
            assert.deepEqual(clone.constraints, []);
            assert.deepEqual(clone.effects, []);
        }
        assert.equal(sourceActor.parent, sourceParent);
        assert.equal(sourceActor.opacity, 0);
        assert.ok(!sourceActor.destroyed);
        assert.deepEqual(overviewGroup.children, [controls]);
        assert.deepEqual(Object.getOwnPropertyDescriptor(BackgroundManager.prototype,
            '_createBackgroundActor'), h.originalDescriptor);
    };
    return h;
}

test('factory posthook preserves this/args/return, construction timing and native objects', async () => {
    const h = await harness();
    assert.equal(h.owner.hasResources, false);
    h.owner.start();
    h.owner.start();
    const workspace = new h.WorkspaceBackground({parent: null});
    const manager = workspace._bgManager;
    const background = manager.backgroundActor;
    const clone = h.card(background);
    assert.ok(clone, 'attach before the owner is parented to the stage');
    assert.equal(h.calls.length, 1, 'original factory called exactly once');
    assert.equal(h.calls[0].manager, manager);
    assert.deepEqual(h.calls[0].args, ['construct', {index: 0}]);
    assert.equal(background, h.actors[0], 'return original native actor');
    assert.equal(background.parent, workspace._backgroundGroup);
    assert.equal(clone.source, h.sourceActor);
    assert.equal(clone.visible, false);
    assert.equal(h.backdrop().visible, false);
    assert.equal(h.clones.length, 2);
    const nativeContent = background.content;
    h.owner.setVisibility(true, true);
    assert.equal(clone.visible, true);
    h.flush();
    assert.equal(h.idles.size, 0);
    assert.equal(h.clones.length, 2, 'deferred backfill must deduplicate');
    h.owner.destroy();
    h.assertClean();
    assert.equal(background.content, nativeContent);
    assert.equal(background.content.rounded_clip_radius, 0, 'native content never mutated');
    assert.ok(!background.destroyed);
    assert.equal(workspace._bgManager, manager);
    assert.equal(workspace.handlers.size, 0);
    assert.equal(background.handlers.size, 0);
});

test('backfills current, pending and fading card backgrounds when overview is already open', async () => {
    const h = await harness({open: true});
    const workspace = new h.WorkspaceBackground();
    const manager = workspace._bgManager;
    const fading = manager.backgroundActor;
    manager.pending();
    manager.swap();
    const current = manager.backgroundActor;
    const pending = manager.pending();
    workspace.nativeAllocate();
    const nativeChildren = [...workspace._backgroundGroup.children];
    const nativeFactoryCalls = h.calls.length;
    // The hook must already be installed when backfill starts enumerating.
    const getChildren = h.overviewGroup.get_children.bind(h.overviewGroup);
    h.overviewGroup.get_children = () => {
        assert.notEqual(h.BackgroundManager.prototype._createBackgroundActor,
            h.originalDescriptor.value);
        return getChildren();
    };
    h.owner.start();
    for (const actor of [fading, current, pending]) {
        assert.ok(h.card(actor));
        assert.equal(h.card(actor).visible, false, 'no privacy decision yet');
    }
    assert.equal(h.backdrop().visible, false);
    h.owner.setVisibility(true, true);
    assert.equal(h.backdrop().visible, true, 'no showing event required');
    assert.equal(h.overviewGroup.visible, true);
    assert.equal(h.clones.length, 4);
    assert.equal(h.calls.length, nativeFactoryCalls, 'never recreate native backgrounds');
    assert.deepEqual(workspace._backgroundGroup.children, nativeChildren);
    h.flush();
    fading.destroy();
    assert.equal(h.clones.filter(clone => !clone.destroyed).length, 3);
    h.owner.destroy();
    h.assertClean();
});

test('local SIZE binding and mask follow native workarea, state, scale and allocation', async () => {
    const h = await harness();
    h.owner.start();
    const workspace = new h.WorkspaceBackground();
    const background = workspace._bgManager.backgroundActor;
    const clone = h.card(background);
    h.owner.setVisibility(true, false);
    assert.equal(clone.visible, true, 'card visibility belongs to the native overview ancestry');
    assert.equal(clone.reactive, false);
    assert.equal(clone.clip_to_allocation, true);
    assert.equal(clone.x, 0);
    assert.equal(clone.y, 0);
    assert.equal(clone.width, 960);
    assert.equal(clone.height, 540);
    assert.equal(clone.constraints[0].source, background);
    assert.equal(clone.constraints[0].coordinate, 5);
    const effect = clone.effects[0];
    assert.deepEqual(effect.uniforms.get('wbMonitorSize'), [1920, 1080]);
    assert.deepEqual(effect.uniforms.get('wbBounds'), [16, 32, 1880, 1020]);
    assert.deepEqual(effect.uniforms.get('wbRadius'), [45]);
    assert.match(effect.snippet.code, /length\(max\(delta/);
    assert.match(effect.snippet.code, /cogl_color_out \*=/);
    workspace.state(0.25);
    assert.deepEqual(effect.uniforms.get('wbRadius'), [15]);
    h.theme.scaleFactor = 1;
    h.theme.emit('notify::scale-factor');
    assert.deepEqual(effect.uniforms.get('wbRadius'), [7.5]);
    // Connect a normal native handler after extension creation: connect_after
    // still sees native parsed facts after the native workarea update.
    const nativeWorkareaSignal = h.display.connect('workareas-changed', () => {
        workspace._workarea = {x: -1880, y: 200, width: 1850, height: 1000};
    });
    h.display.emit('workareas-changed');
    assert.deepEqual(effect.uniforms.get('wbBounds'), [40, 20, 1850, 1000]);
    background.allocate(-30, -40, 1280, 720);
    assert.equal(clone.x, 0);
    assert.equal(clone.y, 0);
    assert.equal(clone.width, 1280);
    assert.equal(clone.height, 720);
    assert.deepEqual(effect.uniforms.get('wbMonitorSize'), [1920, 1080], 'no independent media fitting');
    h.display.disconnect(nativeWorkareaSignal);
    h.owner.destroy();
    h.assertClean();
});

test('unknown mask geometry stays hidden; one deferred constructor backfill is bounded', async () => {
    const h = await harness();
    h.owner.start();
    h.owner.setVisibility(true, true);
    const workspace = new h.WorkspaceBackground({incomplete: true});
    const background = workspace.manager.backgroundActor;
    const clone = h.card(background);
    assert.equal(clone.visible, false, 'manager assignment is still unknown');
    workspace._bgManager = workspace.manager;
    h.flush();
    assert.equal(clone.visible, true);
    delete workspace._workarea;
    workspace.emit('notify::allocation');
    assert.equal(clone.visible, false);
    h.flush();
    assert.equal(h.idles.size, 0, 'no unbounded geometry polling');
    for (const value of [NaN, -1, 2, undefined]) {
        workspace._workarea = {x: -1920, y: 212, width: 1920, height: 1048};
        workspace.state(value);
        assert.equal(clone.visible, false);
    }
    workspace.state(1);
    background.allocated = false;
    workspace.emit('notify::allocation');
    assert.equal(clone.visible, false, 'do not infer allocation from size');
    workspace.nativeAllocate();
    assert.equal(clone.visible, true);
    h.owner.destroy();
    h.assertClean();
});

test('backdrop is first, monitor-local to overviewGroup, nonreactive and privacy gated', async () => {
    const h = await harness();
    h.owner.start();
    const workspace = new h.WorkspaceBackground();
    const card = h.card(workspace._bgManager.backgroundActor);
    const backdrop = h.backdrop();
    assert.deepEqual(h.overviewGroup.children, [backdrop, h.controls]);
    assert.equal(backdrop.reactive, false);
    assert.equal(backdrop.clip_to_allocation, true);
    assert.deepEqual([backdrop.x, backdrop.y, backdrop.width, backdrop.height], [-1920, 180, 1920, 1080]);
    assert.equal(backdrop.source, h.sourceActor);
    for (const [allowed, overview, cardVisible, backdropVisible] of [
        [false, true, false, false], [true, true, true, true],
        [false, false, false, false], [true, false, true, false],
        [false, true, false, false], [true, true, true, true],
        [1, true, false, false], [true, 1, true, false],
    ]) {
        h.owner.setVisibility(allowed, overview);
        assert.equal(card.visible, cardVisible);
        assert.equal(backdrop.visible, backdropVisible);
    }
    assert.equal(h.overviewGroup.visible, false, 'never toggle native overview');
    h.owner.destroy();
    h.owner.setVisibility(true, true);
    h.owner.start();
    h.assertClean();
});

test('FixedLayout keeps backdrop allocation on the selected monitor after card geometry is ready', async () => {
    for (const [x, y] of [[1920, 180], [-1920, 180], [1920, -1080], [-1920, -1080], [0, 1080]]) {
        const monitorGeometry = {x, y, width: 1920, height: 1080};
        const h = await harness({open: true, monitorGeometry});
        // An unrelated source preferred size must not override the clone's request.
        h.sourceActor.set_size(3840, 2160);
        const workspace = new h.WorkspaceBackground();
        const nativeLayout = h.overviewGroup.layout_manager;
        h.owner.start();
        h.owner.setVisibility(true, true);
        h.flush();
        const card = h.card(workspace._bgManager.backgroundActor);
        const backdrop = h.backdrop();
        assert.equal(card.visible, true, 'native card geometry is known');
        assert.deepEqual(card.effects[0].uniforms.get('wbBounds'), [16, 32, 1880, 1020]);
        for (const [width, height] of [[4480, 1440], [7680, 4320], [800, 600]]) {
            h.overviewGroup.allocate(0, 0, width, height);
            workspace.nativeAllocate();
            workspace.state(1);
            h.owner.setVisibility(true, true);
            assert.deepEqual(backdrop.allocation, monitorGeometry,
                `selected monitor (${x}, ${y}), parent ${width}x${height}`);
            assert.deepEqual(backdrop.fixedPosition, [x, y]);
            assert.deepEqual(backdrop.sizeRequest, [1920, 1080]);
            assert.equal(backdrop.clip_to_allocation, true);
            assert.equal(backdrop.reactive, false);
            assert.equal(backdrop.visible, true);
            assert.equal(h.overviewGroup.children[0], backdrop);
            assert.equal(h.overviewGroup.layout_manager, nativeLayout, 'no native layout changes');
        }
        h.owner.destroy();
        h.assertClean();
        assert.equal(h.overviewGroup.layout_manager, nativeLayout);
    }
});

test('strict owner type and ancestry deny desktop, unlock, shield, login and lookalikes', async () => {
    const h = await harness();
    const foreignRoots = ['desktop', 'UnlockDialog', 'screen-shield', 'login', 'small-thumbnail'];
    const foreignManagers = [];
    for (const name of foreignRoots) {
        const owner = new Actor({name});
        owner._monitorIndex = 0;
        owner._bin = new Actor();
        owner._backgroundGroup = new Actor();
        owner._bin.add_child(owner._backgroundGroup);
        owner.add_child(owner._bin);
        h.controls.add_child(owner);
        owner._bgManager = new h.BackgroundManager(owner._backgroundGroup);
        foreignManagers.push(owner._bgManager);
    }
    const existing = new h.WorkspaceBackground();
    const wrongActor = new h.BackgroundActor(1);
    existing._backgroundGroup.add_child(wrongActor);
    existing.nativeAllocate();
    h.owner.start();
    h.owner.setVisibility(true, true);
    assert.ok(h.card(existing._bgManager.backgroundActor));
    assert.equal(h.card(wrongActor), undefined, 'actor monitor must also match');
    for (const manager of foreignManagers) {
        assert.equal(h.card(manager.backgroundActor), undefined, 'deny unrelated existing owners');
        assert.equal(h.card(manager.pending()), undefined, 'deny unrelated new factory actors');
    }
    const wrongMonitor = new h.WorkspaceBackground({index: 1});
    assert.equal(h.card(wrongMonitor._bgManager.backgroundActor), undefined);
    const wrongShape = new h.WorkspaceBackground();
    h.card(wrongShape._bgManager.backgroundActor).destroy();
    wrongShape._bin = new Actor();
    assert.equal(h.card(wrongShape._bgManager.pending()), undefined, 'field identities are mandatory');
    const wrongManager = new h.WorkspaceBackground();
    h.card(wrongManager._bgManager.backgroundActor).destroy();
    const duplicate = new h.BackgroundManager(wrongManager._backgroundGroup);
    assert.equal(h.card(duplicate.backgroundActor), undefined, 'must be the concrete owner manager');
    const manager = wrongManager._bgManager;
    manager._monitorIndex = '0';
    assert.equal(h.card(manager.pending()), undefined, 'monitor index is not coerced');
    manager._monitorIndex = 0;
    manager._layoutManager = {};
    assert.equal(h.card(manager.pending()), undefined, 'layout manager identity must also match');
    assert.equal(h.errors.length, 0);
    h.owner.destroy();
    h.assertClean();
});

test('monitor object replacement is rejected before monitors-changed, even at the same index', async () => {
    const h = await harness();
    h.owner.start();
    const existing = new h.WorkspaceBackground();
    h.owner.setVisibility(true, true);
    const clone = h.card(existing._bgManager.backgroundActor);
    h.layoutManager.monitors[0] = {...h.monitor};
    const replacement = new h.WorkspaceBackground();
    assert.equal(h.card(replacement._bgManager.backgroundActor), undefined);
    assert.equal(clone.visible, false);
    assert.equal(h.backdrop().visible, false);
    h.owner.setVisibility(true, true);
    assert.equal(clone.visible, false);
    h.layoutManager.emit('monitors-changed');
    assert.equal(clone.visible, false);
    h.owner.destroy();
    h.assertClean();
});

test('background replacement, pending supersession, fading, destruction and dedup use actor identity', async () => {
    const h = await harness();
    h.owner.start();
    h.owner.setVisibility(true, true);
    const workspace = new h.WorkspaceBackground();
    const manager = workspace._bgManager;
    const first = manager.backgroundActor;
    const firstClone = h.card(first);
    const superseded = manager.pending();
    const supersededClone = h.card(superseded);
    const pending = manager.pending();
    assert.ok(supersededClone.destroyed);
    manager.swap();
    workspace.nativeAllocate();
    h.flush();
    const pendingClone = h.card(pending);
    for (let i = 0; i < 3; i++) {
        workspace.emit('notify::allocation');
        h.owner.setVisibility(true, true);
    }
    assert.equal(h.card(first), firstClone);
    assert.equal(h.card(pending), pendingClone);
    assert.equal(firstClone.visible, true, 'retain the native fading actor until native destruction');
    first.destroy();
    assert.ok(firstClone.destroyed);
    assert.equal(first.handlers.size, 0);
    const last = manager.pending();
    manager.swap(true);
    assert.ok(pendingClone.destroyed, 'immediate native destruction is also pruned');
    const lastClone = h.card(last);
    lastClone.destroy();
    assert.equal(last.handlers.size, 0, 'external clone destruction releases parent tracking');
    assert.equal(workspace.handlers.size, 0);
    assert.ok(!last.destroyed, 'never destroy the native parent');
    assert.equal(h.idles.size, 0);
    h.owner.destroy();
    h.assertClean();
});

test('native parent removal or owner destruction prunes only extension resources', async () => {
    const h = await harness();
    h.owner.start();
    const workspace = new h.WorkspaceBackground();
    const background = workspace._bgManager.backgroundActor;
    const clone = h.card(background);
    workspace._backgroundGroup.remove_child(background);
    assert.ok(clone.destroyed);
    assert.ok(!background.destroyed);
    const other = new h.WorkspaceBackground();
    const otherClone = h.card(other._bgManager.backgroundActor);
    other.destroy();
    assert.ok(otherClone.destroyed);
    h.owner.destroy();
    h.assertClean();
});

test('cleanup retains failed clone/signal/idle/effect/constraint ownership and retries after disable', async () => {
    const h = await harness();
    h.owner.start();
    const workspace = new h.WorkspaceBackground();
    h.owner.setVisibility(true, true);
    const background = workspace._bgManager.backgroundActor;
    const clone = h.card(background);
    clone.destroyError = clone.effectError = clone.constraintError = true;
    background.disconnectError = true;
    h.theme.disconnectError = true;
    h.idleRemoveError = true;
    assert.throws(() => h.owner.destroy(), /Overview cleanup failed/);
    assert.equal(h.owner.hasResources, true);
    assert.equal(clone.visible, false);
    assert.equal(clone.source, null);
    assert.equal(clone.effects.length, 1);
    assert.equal(clone.constraints.length, 1);
    assert.equal(h.idles.size, 1);
    assert.ok(background.handlers.size > 0);
    assert.deepEqual(Object.getOwnPropertyDescriptor(h.BackgroundManager.prototype,
        '_createBackgroundActor'), h.originalDescriptor, 'hook already deactivated/restored');
    h.owner.setVisibility(true, true);
    const later = new h.WorkspaceBackground();
    assert.equal(h.card(later._bgManager.backgroundActor), undefined);
    clone.destroyError = clone.effectError = clone.constraintError = false;
    background.disconnectError = false;
    h.theme.disconnectError = false;
    h.idleRemoveError = false;
    h.owner.destroy();
    h.owner.destroy();
    h.assertClean();
    assert.ok(!background.destroyed);
});

test('hook restoration failure stays owned and inactive until a later cleanup succeeds', async () => {
    const h = await harness();
    h.owner.start();
    const prototype = h.BackgroundManager.prototype;
    const wrapper = prototype._createBackgroundActor;
    // A proxy on the owned slot simulates Object.defineProperty failing on restore.
    const hook = h.owner._hook;
    hook.prototype = new Proxy(prototype, {defineProperty() { throw new Error('restore failed'); }});
    assert.throws(() => h.owner.destroy(), /restore failed/);
    assert.equal(h.owner.hasResources, true);
    const workspace = new h.WorkspaceBackground();
    assert.equal(prototype._createBackgroundActor, wrapper);
    assert.equal(h.card(workspace._bgManager.backgroundActor), undefined);
    hook.prototype = prototype;
    h.owner.destroy();
    h.assertClean();
});

test('an idle that could not be cancelled releases ownership when it naturally runs', async () => {
    const h = await harness();
    h.owner.start();
    new h.WorkspaceBackground();
    h.idleRemoveError = true;
    assert.throws(() => h.owner.destroy(), /idle removal failed/);
    assert.equal(h.owner.hasResources, true);
    assert.equal(h.idles.size, 1);
    h.flush();
    h.assertClean();
});

test('owned source loss reports failure without discovering or substituting another source', async () => {
    const h = await harness();
    h.owner.start();
    const workspace = new h.WorkspaceBackground();
    h.owner.setVisibility(true, true);
    h.sourceActor.destroy();
    assert.equal(h.errors.length, 1);
    assert.match(h.errors[0].message, /source actor was destroyed/);
    assert.ok(h.clones.every(clone => !clone.visible));
    const later = new h.WorkspaceBackground();
    assert.equal(h.card(later._bgManager.backgroundActor), undefined);
    h.owner.destroy();
    assert.equal(h.owner.hasResources, false);
    assert.equal(h.idles.size, 0);
    assert.equal(workspace.handlers.size, 0);
    assert.equal(workspace._bgManager.backgroundActor.handlers.size, 0);
    assert.ok(!workspace._bgManager.backgroundActor.destroyed);
});

test('extension failures report once and never escape the native factory, including a throwing reporter', async () => {
    const h = await harness({onError: () => { throw new Error('reporter failed'); }});
    h.owner.start();
    h.owner.setVisibility(true, true);
    h.maskError = true;
    let workspace;
    assert.doesNotThrow(() => { workspace = new h.WorkspaceBackground(); });
    assert.equal(h.errors.length, 1);
    assert.match(h.errors[0].message, /mask creation failed/);
    assert.equal(workspace._bgManager.backgroundActor, h.actors[0]);
    assert.equal(h.backdrop().visible, false);
    assert.equal(h.clones[1].visible, false, 'partially constructed clone remains hidden and owned');
    assert.equal(h.clones[1].parent, null);
    assert.match(h.warnings[0], /reporter failed/);
    new h.WorkspaceBackground();
    assert.equal(h.errors.length, 1);
    h.owner.destroy();
    h.assertClean();
});

test('startup and later geometry failures retain resources for the session owner to clean', async () => {
    for (const failure of ['cloneError', 'idleAddError', 'uniformError']) {
        const h = await harness({onError: owner => owner.owner.destroy()});
        if (failure === 'cloneError')
            h.cloneError = true;
        h.owner.start();
        if (failure !== 'cloneError') {
            const workspace = new h.WorkspaceBackground();
            h[failure] = true;
            if (failure === 'uniformError')
                workspace.state(1);
            else
                workspace._bgManager.pending();
        }
        assert.equal(h.errors.length, 1, failure);
        h.assertClean();
    }
});

test('native factory failures propagate unchanged, without being reclassified as clone failures', async () => {
    const h = await harness();
    h.owner.start();
    h.nativeError = true;
    assert.throws(() => new h.WorkspaceBackground(), /native factory failed/);
    assert.equal(h.calls.length, 1);
    assert.equal(h.errors.length, 0);
    h.owner.destroy();
    h.assertClean();
});

test('preexisting foreign wrapper and full descriptor are preserved', async () => {
    let foreignCalls = 0;
    const h = await harness({beforeHook: (prototype, original) => {
        Object.defineProperty(prototype, '_createBackgroundActor', {
            configurable: true, writable: false, enumerable: true,
            value: function (...args) {
                foreignCalls++;
                return original.apply(this, args);
            },
        });
    }});
    h.owner.start();
    const workspace = new h.WorkspaceBackground();
    assert.equal(foreignCalls, 1);
    assert.ok(h.card(workspace._bgManager.backgroundActor));
    h.owner.destroy();
    h.assertClean();
    new h.WorkspaceBackground();
    assert.equal(foreignCalls, 2);
});

test('later foreign wrapper survives disable; embedded old hook is reference-free pass-through', async () => {
    const h = await harness();
    h.owner.start();
    new h.WorkspaceBackground();
    const hook = h.owner._hook;
    const prototype = h.BackgroundManager.prototype;
    const wrapper = prototype._createBackgroundActor;
    let foreignCalls = 0;
    const foreign = function (...args) {
        foreignCalls++;
        return wrapper.apply(this, args);
    };
    prototype._createBackgroundActor = foreign;
    h.owner.destroy();
    assert.equal(prototype._createBackgroundActor, foreign);
    assert.equal(hook.state.owner, null, 'retained hook has no session/source/extension owner');
    assert.equal(h.owner._source, null);
    assert.equal(h.owner._onError, null);
    assert.equal(h.owner.hasResources, false);
    const cloneCount = h.clones.length;
    const workspace = new h.WorkspaceBackground();
    assert.equal(foreignCalls, 1);
    assert.equal(h.card(workspace._bgManager.backgroundActor), undefined);
    assert.equal(h.clones.length, cloneCount);
    // The foreign extension, not OverviewClones, owns this final restoration.
    Object.defineProperty(prototype, '_createBackgroundActor', h.originalDescriptor);
    h.assertClean();
});

test('a later replacement is not overwritten, even when it does not chain the installed hook', async () => {
    const h = await harness();
    h.owner.start();
    const prototype = h.BackgroundManager.prototype;
    const original = h.originalDescriptor.value;
    const replacement = function (...args) { return original.apply(this, args); };
    prototype._createBackgroundActor = replacement;
    h.owner.destroy();
    assert.equal(h.owner.hasResources, false);
    assert.equal(prototype._createBackgroundActor, replacement);
    const workspace = new h.WorkspaceBackground();
    assert.equal(h.card(workspace._bgManager.backgroundActor), undefined);
    Object.defineProperty(prototype, '_createBackgroundActor', h.originalDescriptor);
    h.assertClean();
});
