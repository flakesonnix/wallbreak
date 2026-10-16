// SPDX-License-Identifier: MIT
import Clutter from 'gi://Clutter';
import Cogl from 'gi://Cogl';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';
import * as Background from 'resource:///org/gnome/shell/ui/background.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as Workspace from 'resource:///org/gnome/shell/ui/workspace.js';

// Independently authored signed-distance mask, not Mutter's content shader.
// BackgroundContent clips its own pixels, not a BackgroundActor's children.
const CardMask = GObject.registerClass(
class WallbreakOverviewCardMask extends Shell.GLSLEffect {
    _init() {
        super._init();
        this._uniforms = ['wbMonitorSize', 'wbBounds', 'wbRadius'].map(name => {
            const location = this.get_uniform_location(name);
            if (location < 0)
                throw new Error(`Overview mask uniform unavailable: ${name}`);
            return location;
        });
    }

    vfunc_build_pipeline() {
        this.add_glsl_snippet(Cogl.SnippetHook.FRAGMENT, `
            uniform vec2 wbMonitorSize;
            uniform vec4 wbBounds;
            uniform float wbRadius;
        `, `
            vec2 halfSize = wbBounds.zw * 0.5;
            vec2 center = wbBounds.xy + halfSize;
            float r = min(wbRadius, min(halfSize.x, halfSize.y));
            vec2 delta = abs(cogl_tex_coord0_in.xy * wbMonitorSize - center)
                - halfSize + vec2(r);
            float distance = length(max(delta, vec2(0.0)))
                + min(max(delta.x, delta.y), 0.0) - r;
            cogl_color_out *= clamp(0.5 - distance, 0.0, 1.0);
        `, false);
    }

    setGeometry(monitor, workarea, radius) {
        this.set_uniform_float(this._uniforms[0], 2, [monitor.width, monitor.height]);
        this.set_uniform_float(this._uniforms[1], 4, [
            workarea.x - monitor.x, workarea.y - monitor.y, workarea.width, workarea.height,
        ]);
        this.set_uniform_float(this._uniforms[2], 1, [radius]);
        this.queue_repaint();
    }
});

function forgetSignals(signals, object) {
    for (let i = signals.length - 1; i >= 0; i--) {
        if (signals[i][0] === object)
            signals.splice(i, 1);
    }
}

// This closure must not capture OverviewClones: a later extension may keep it.
function factoryHook(original, state) {
    return function (...args) {
        const actor = original.apply(this, args);
        state.owner?._attempt(() => state.owner._attach(this, actor));
        return actor;
    };
}

export class OverviewClones {
    constructor(sourceActor, monitorObject, monitorIndex, onError) {
        this._source = sourceActor;
        this._monitor = monitorObject;
        this._index = monitorIndex;
        this._onError = onError;
        this._active = false;
        this._destroyed = false;
        this._failed = false;
        this._allowed = false;
        this._overviewVisible = false;
        this._hook = null;
        this._signals = [];
        // Actor identity includes the manager's current, pending and fading actors.
        // The null key owns the single full-monitor overview backdrop.
        this._records = new Map();
    }

    get hasResources() {
        return this._hook !== null || this._signals.length > 0 || this._records.size > 0;
    }

    start() {
        if (this._active || this._destroyed || this._failed)
            return;
        this._active = true;
        this._attempt(() => {
            if (!this._monitorCurrent())
                throw new Error('Selected overview monitor is no longer current');
            const prototype = Background.BackgroundManager.prototype;
            const descriptor = Object.getOwnPropertyDescriptor(prototype, '_createBackgroundActor');
            if (typeof descriptor?.value !== 'function')
                throw new Error('GNOME background factory is unavailable');
            const state = {owner: this};
            const wrapper = factoryHook(descriptor.value, state);
            this._hook = {prototype, descriptor, wrapper, state};
            Object.defineProperty(prototype, '_createBackgroundActor', {...descriptor, value: wrapper});

            const theme = St.ThemeContext.get_for_stage(global.stage);
            this._connect(this._signals, theme, 'notify::scale-factor', () => this._sync());
            this._connect(this._signals, global.display, 'workareas-changed', () => this._sync());
            this._connect(this._signals, Main.layoutManager, 'monitors-changed', () => this._sync());
            this._connect(this._signals, this._source, 'destroy', () => {
                throw new Error('Owned overview source actor was destroyed');
            });

            const backdrop = this._newRecord(null);
            const {x, y, width, height} = this._monitor;
            // GNOME 50.2 overviewGroup is an St.Widget with the default
            // FixedLayout: allocate the fixed position and explicit size request,
            // independently of the stage allocation (not BinLayout alignment).
            backdrop.clone.set_position(x, y);
            backdrop.clone.set_size(width, height);
            Main.layoutManager.overviewGroup.insert_child_at_index(backdrop.clone, 0);

            // Hook first; backfill without recreating backgrounds or toggling overview.
            const visit = actor => {
                if (actor instanceof Workspace.WorkspaceBackground) {
                    const manager = actor._bgManager;
                    if (manager?._container === actor._backgroundGroup) {
                        for (const child of manager._container.get_children())
                            this._attach(manager, child);
                    }
                }
                for (const child of actor.get_children())
                    visit(child);
            };
            visit(Main.layoutManager.overviewGroup);
            this._sync();
        });
    }

    setVisibility(allowed, overviewVisible) {
        this._allowed = allowed === true;
        this._overviewVisible = overviewVisible === true;
        if (this._active)
            this._attempt(() => this._sync());
    }

    _monitorCurrent() {
        return Number.isInteger(this._index) && this._index >= 0 && this._monitor &&
            Main.layoutManager.monitors[this._index] === this._monitor;
    }

    _workspaceOwner(manager, actor) {
        if (!this._monitorCurrent() || manager._monitorIndex !== this._index ||
            manager._layoutManager !== Main.layoutManager ||
            !(actor instanceof Meta.BackgroundActor) || actor.monitor !== this._index)
            return null;
        const group = manager._container;
        const bin = group?.get_parent();
        const owner = bin?.get_parent();
        if (!(owner instanceof Workspace.WorkspaceBackground) ||
            owner._monitorIndex !== this._index || owner._bin !== bin ||
            owner._backgroundGroup !== group || actor.get_parent() !== group ||
            (owner._bgManager !== undefined && owner._bgManager !== manager))
            return null;
        // _bgManager is assigned only after the factory returns during construction.
        return owner;
    }

    _newRecord(background, manager = null, owner = null) {
        const record = {background, manager, owner, clone: null, effect: null,
            constraint: null, signals: [], idle: 0, retired: false};
        this._records.set(background, record);
        record.clone = new Clutter.Clone({
            source: this._source, reactive: false, visible: false,
            x: 0, y: 0, clip_to_allocation: true,
        });
        const clone = record.clone;
        this._connect(record.signals, clone, 'destroy', () => {
            record.clone = null;
            record.constraint = null;
            record.effect = null;
            if (!record.retired)
                this._cleanupRecord(record);
        });
        return record;
    }

    _attach(manager, background) {
        if (!this._active)
            return;
        if (!this._monitorCurrent()) {
            this._sync();
            return;
        }
        const owner = this._workspaceOwner(manager, background);
        if (!owner || this._records.has(background))
            return;
        const record = this._newRecord(background, manager, owner);
        record.constraint = new Clutter.BindConstraint({
            source: background, coordinate: Clutter.BindCoordinate.SIZE, offset: 0,
        });
        record.clone.add_constraint(record.constraint);
        record.effect = new CardMask();
        record.clone.add_effect(record.effect);
        this._connect(record.signals, background, 'destroy', () => this._cleanupRecord(record));
        this._connect(record.signals, owner, 'destroy', () => this._cleanupRecord(record));
        this._connect(record.signals, background, 'parent-set', () => this._syncCard(record));
        this._connect(record.signals, background, 'notify::allocation', () => this._syncCard(record));
        this._connect(record.signals, owner, 'notify::allocation', () => this._syncCard(record));
        // This property is bound by native code to the owner's adjustment.
        this._connect(record.signals, owner, 'notify::state-adjustment-value', () => this._syncCard(record));
        background.add_child(record.clone);
        // One owned idle crosses the synchronous native constructor/assignment.
        // Missing fields remain hidden, with allocation/state signals providing backfill.
        record.idle = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            record.idle = 0;
            if (record.retired)
                this._attempt(() => this._cleanupRecord(record));
            else if (this._active)
                this._attempt(() => this._syncCard(record));
            return GLib.SOURCE_REMOVE;
        });
        this._syncCard(record);
    }

    _syncCard(record) {
        if (record.retired || !record.clone)
            return;
        const {owner, background, manager} = record;
        if (!this._active || !this._monitorCurrent()) {
            record.clone.visible = false;
            return;
        }
        if (this._workspaceOwner(manager, background) !== owner) {
            this._cleanupRecord(record);
            return;
        }
        const workarea = owner._workarea;
        const state = owner._stateAdjustment?.value;
        const scale = St.ThemeContext.get_for_stage(global.stage).scaleFactor;
        // Meta 18 has no rounded-bounds getter. Use the native owner's parsed
        // workarea and GNOME 50.2's 30 * theme scale * adjustment radius contract.
        if (owner._bgManager !== manager || !workarea ||
            ![workarea.x, workarea.y, workarea.width, workarea.height, state, scale,
                this._monitor.x, this._monitor.y, this._monitor.width, this._monitor.height,
                background.width, background.height].every(Number.isFinite) ||
            workarea.width <= 0 || workarea.height <= 0 || scale <= 0 || state < 0 || state > 1 ||
            this._monitor.width <= 0 || this._monitor.height <= 0 ||
            !background.has_allocation() || background.width <= 0 || background.height <= 0) {
            record.clone.visible = false;
            return;
        }
        record.effect.setGeometry(this._monitor, workarea, 30 * scale * state);
        record.clone.visible = this._allowed;
    }

    _sync() {
        for (const record of this._records.values()) {
            if (record.background) {
                this._syncCard(record);
            } else if (record.clone && !record.retired) {
                record.clone.visible = this._active && this._allowed &&
                    this._overviewVisible && Boolean(this._monitorCurrent());
            }
        }
    }

    _connect(signals, object, name, callback) {
        const id = object.connect_after(name, () => {
            if (name === 'destroy')
                forgetSignals(signals, object);
            this._attempt(callback);
        });
        signals.push([object, id]);
    }

    _attempt(action) {
        try {
            action();
        } catch (error) {
            if (this._failed || this._destroyed)
                return;
            this._failed = true;
            this._active = false;
            if (this._hook)
                this._hook.state.owner = null;
            for (const record of this._records.values()) {
                try {
                    if (record.clone)
                        record.clone.visible = false;
                } catch {
                    // Keep ownership for destroy() to retry even when hiding fails.
                }
            }
            try {
                this._onError?.(error);
            } catch (reportError) {
                console.error(`Wallbreak overview failure callback: ${reportError.message}`);
            }
        }
    }

    _disconnect(signals, errors) {
        for (const entry of [...signals]) {
            try {
                entry[0].disconnect(entry[1]);
                signals.splice(signals.indexOf(entry), 1);
            } catch (error) {
                errors.push(error);
            }
        }
    }

    _cleanupRecord(record) {
        record.retired = true;
        const errors = [];
        const release = action => {
            try {
                action();
            } catch (error) {
                errors.push(error);
            }
        };
        if (record.idle) {
            release(() => {
                GLib.source_remove(record.idle);
                record.idle = 0;
            });
        }
        this._disconnect(record.signals, errors);
        const clone = record.clone;
        if (clone) {
            release(() => { clone.visible = false; });
            if (record.constraint) {
                release(() => {
                    clone.remove_constraint(record.constraint);
                    record.constraint = null;
                });
            }
            if (record.effect) {
                release(() => {
                    clone.remove_effect(record.effect);
                    record.effect = null;
                });
            }
            release(() => { clone.source = null; });
            release(() => {
                clone.destroy();
                record.clone = null;
                record.constraint = null;
                record.effect = null;
                forgetSignals(record.signals, clone);
            });
        }
        if (!record.clone && !record.idle && record.signals.length === 0)
            this._records.delete(record.background);
        if (errors.length)
            throw new Error(`Overview clone cleanup failed: ${errors.map(error => error.message).join('; ')}`);
    }

    destroy() {
        this._active = false;
        this._destroyed = true;
        this._allowed = false;
        // Deactivate before restoring or cleaning up. Foreign wrappers may retain
        // our function, but must never retain this session or its renderer source.
        if (this._hook)
            this._hook.state.owner = null;
        this._source = null;
        this._monitor = null;
        this._onError = null;
        const errors = [];
        if (this._hook) {
            try {
                const {prototype, descriptor, wrapper} = this._hook;
                if (Object.getOwnPropertyDescriptor(prototype, '_createBackgroundActor')?.value === wrapper)
                    Object.defineProperty(prototype, '_createBackgroundActor', descriptor);
                this._hook = null;
            } catch (error) {
                errors.push(error);
            }
        }
        this._disconnect(this._signals, errors);
        for (const record of this._records.values()) {
            try {
                this._cleanupRecord(record);
            } catch (error) {
                errors.push(error);
            }
        }
        if (errors.length)
            throw new Error(`Overview cleanup failed: ${errors.map(error => error.message).join('; ')}`);
    }
}
