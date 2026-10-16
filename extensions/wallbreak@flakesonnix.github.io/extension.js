// SPDX-License-Identifier: MIT
// Original Wallbreak integration. No renderer installation or settings writes.
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import {
    BUS_NAME, OBJECT_PATH, INTERFACE, BridgeError, StderrTail,
    validateRequest, frameGeometry, rendererArgs, matchesRenderer,
} from './protocol.mjs';
import {OverviewClones} from './overview.mjs';

const READY_MS = 20_000;
const TERM_MS = 3_000;
const DRAIN_MS = 1_000;

export default class WallbreakExtension extends Extension {
    enable() {
        if (GLib.getenv('XDG_SESSION_TYPE') !== 'wayland')
            throw new Error('Wallbreak requires GNOME 50 on Wayland');
        this._generation ??= 0;
        this._session ??= null;
        const exportGeneration = this._exportGeneration = (this._exportGeneration ?? 0) + 1;
        const [ok, xml] = this.dir.get_child('dbus.xml').load_contents(null);
        if (!ok)
            throw new Error('Cannot load Wallbreak D-Bus interface');
        this._connection = Gio.DBus.session;
        this._exported = Gio.DBusExportedObject.wrapJSObject(new TextDecoder().decode(xml), this);
        this._exported.export(this._connection, OBJECT_PATH);
        this._enabled = true;
        this._nameReady = false;
        this._nameId = Gio.bus_own_name_on_connection(
            this._connection, BUS_NAME, Gio.BusNameOwnerFlags.DO_NOT_QUEUE,
            () => {
                if (this._enabled && this._exportGeneration === exportGeneration)
                    this._nameReady = true;
            },
            () => {
                if (!this._enabled || this._exportGeneration !== exportGeneration)
                    return;
                this._nameReady = false;
                if (this._session)
                    this._stop(this._session, 'Wallbreak D-Bus name was lost', true);
                this._exported?.unexport();
                console.error('Wallbreak could not retain its D-Bus name');
            });
    }

    disable() {
        this._enabled = false;
        this._exportGeneration++;
        this._nameReady = false;
        this._exported?.unexport();
        this._exported = null;
        if (this._nameId)
            Gio.bus_unown_name(this._nameId);
        this._nameId = 0;
        this._connection = null;
        const session = this._session;
        if (session) {
            session.discardFinal = true;
            this._unwatchOwner(session);
            this._stop(session, 'Extension disabled');
            if (session.settled)
                this._session = null;
        }
        // The pending wait/read callbacks retain this owner until the child is reaped.
    }

    _reply(invocation, signature, action) {
        try {
            const sender = invocation.get_sender();
            if (!sender?.startsWith(':'))
                throw new BridgeError('NotOwner', 'A unique D-Bus sender is required');
            invocation.return_value(new GLib.Variant(signature, action(sender)));
        } catch (error) {
            invocation.return_dbus_error(
                `${INTERFACE}.Error.${error instanceof BridgeError ? error.code : 'Failed'}`,
                String(error.message).replaceAll('\0', '\\0'));
        }
    }

    InfoAsync(_params, invocation) {
        this._reply(invocation, '(u)', () => [1]);
    }

    StartAsync(params, invocation) {
        this._reply(invocation, '(s)', sender => [this._start(params, sender)]);
    }

    StatusAsync([token], invocation) {
        this._reply(invocation, '(ss)', sender => {
            const session = this._ownedSession(token, sender);
            return [session.state, this._detail(session)];
        });
    }

    StopAsync([token], invocation) {
        this._reply(invocation, '()', sender => {
            const session = this._ownedSession(token, sender);
            session.stopRequested = true;
            this._stop(session, 'Stop requested');
            return [];
        });
    }

    _ownedSession(token, sender) {
        const session = this._session;
        if (!session || session.token !== token || session.ownerGone)
            throw new BridgeError('UnknownSession', 'Unknown Wallbreak session');
        if (session.owner !== sender)
            throw new BridgeError('NotOwner', 'Session belongs to another D-Bus sender');
        return session;
    }

    _start(params, owner) {
        if (!this._enabled || !this._nameReady)
            throw new BridgeError('Unavailable', 'Wallbreak bridge is not available');
        if (this._session && !this._session.settled)
            throw new BridgeError('Busy', 'A renderer is still active or being reaped');
        const request = validateRequest(params);
        const file = Gio.File.new_for_path(request.renderer);
        const info = file.query_info('standard::type,access::can-execute', Gio.FileQueryInfoFlags.NONE, null);
        if (info.get_file_type() !== Gio.FileType.REGULAR ||
            !info.get_attribute_boolean('access::can-execute'))
            throw new BridgeError('InvalidArgument', 'Renderer must be an existing executable file');
        const index = request.output === '' ? Main.layoutManager.primaryIndex :
            global.backend.get_monitor_manager().get_monitor_for_connector(request.output);
        const monitor = Main.layoutManager.monitors[index];
        if (!Number.isInteger(index) || index < 0 || !monitor)
            throw new BridgeError('InvalidArgument', 'Requested output is not available');
        const geometry = frameGeometry(monitor, global.display.get_monitor_scale(index));
        const manager = Main.layoutManager._bgManagers[index];
        if (!manager?.backgroundActor)
            throw new BridgeError('Unavailable', 'Desktop background is not ready');

        if (this._session)
            this._unwatchOwner(this._session);
        const session = {
            token: GLib.uuid_string_random(), owner, generation: ++this._generation,
            state: 'starting', detail: 'Waiting for owned renderer window',
            geometry, manager, monitor, monitorIndex: index,
            signals: [], candidates: new Map(), pendingWindows: [],
            tail: new StderrTail(), warnings: new Set(), reaped: false, stderrDone: false,
            settled: false, overview: Main.overview.visible || Main.overview.visibleTarget,
        };
        this._session = session;
        try {
            session.ownerWatch = Gio.bus_watch_name_on_connection(
                this._connection, owner, Gio.BusNameWatcherFlags.NONE, null, () => {
                    if (!this._current(session))
                        return;
                    session.ownerGone = true;
                    this._unwatchOwner(session);
                    this._stop(session, 'D-Bus owner disconnected');
                    if (session.settled)
                        this._session = null;
                });
            // Subscribe before spawning: window creation and metadata can arrive separately.
            this._connect(session, global.display, 'window-created', (_display, window) => {
                if (session.client)
                    this._observeWindow(session, window);
                else
                    session.pendingWindows.push(window);
            });
            this._connect(session, Main.layoutManager, 'monitors-changed', () =>
                this._stop(session, 'Monitor configuration changed; start a new session', true));
            this._connect(session, manager, 'changed', () => this._attachClone(session));
            this._connect(session, Main.overview, 'showing', () => {
                session.overview = true;
                this._updateVisibility(session);
            });
            this._connect(session, Main.overview, 'hidden', () => {
                session.overview = false;
                this._updateVisibility(session);
            });
            this._connect(session, Main.sessionMode, 'updated', () => this._updateVisibility(session));
            this._connect(session, Main.layoutManager.screenShieldGroup, 'notify::visible', () =>
                this._updateVisibility(session));
            session.readyTimer = this._timer(READY_MS, () => {
                session.readyTimer = 0;
                this._stop(session, 'Renderer readiness timed out after 20 seconds', true);
            });

            const launcher = new Gio.SubprocessLauncher({
                flags: Gio.SubprocessFlags.STDOUT_SILENCE | Gio.SubprocessFlags.STDERR_PIPE,
            });
            try {
                session.client = Meta.WaylandClient.new_subprocess(
                    global.context, launcher, rendererArgs(request, session.token, geometry));
                session.process = session.client.get_subprocess();
                this._wait(session);
            } finally {
                launcher.close();
            }
            session.pid = session.client.get_pid();
            session.stderr = session.process.get_stderr_pipe();
            session.readCancel = new Gio.Cancellable();
            this._readStderr(session);
            for (const window of session.pendingWindows)
                this._observeWindow(session, window);
            session.pendingWindows = [];
        } catch (error) {
            if (!session.process) {
                session.reaped = true;
                session.stderrDone = true;
            } else if (!session.stderr) {
                session.stderrDone = true;
            }
            this._stop(session, `Renderer startup failed: ${error.message}`, true);
        }
        // Even spawn failures have an owned token so Status retains the diagnostics.
        return session.token;
    }

    _current(session) {
        return this._session === session && this._generation === session.generation;
    }

    _live(session) {
        return this._current(session) && this._enabled &&
            (session.state === 'starting' || session.state === 'running');
    }

    _connect(session, object, name, callback) {
        const id = object.connect(name, (...args) => {
            if (!this._live(session))
                return;
            try {
                callback(...args);
            } catch (error) {
                this._stop(session, `Shell integration failed: ${error.message}`, true);
            }
        });
        session.signals.push([object, id]);
    }

    _observeWindow(session, window) {
        if (!this._live(session) || session.candidates.has(window) ||
            window.get_client_type() !== Meta.WindowClientType.WAYLAND ||
            !session.client.owns_window(window))
            return;
        const candidate = {shown: false};
        session.candidates.set(window, candidate);
        this._connect(session, window, 'notify::title', () => this._matchWindow(session, window));
        this._connect(session, window, 'notify::wm-class', () => this._matchWindow(session, window));
        this._connect(session, window, 'shown', () => {
            candidate.shown = true;
            this._matchWindow(session, window);
        });
        this._connect(session, window, 'unmanaged', () => {
            if (session.window === window)
                this._stop(session, 'Owned renderer window was unmanaged', true);
            session.candidates.delete(window);
        });
        this._matchWindow(session, window);
    }

    _matchWindow(session, window) {
        const native = window.get_client_type() === Meta.WindowClientType.WAYLAND;
        const matches = matchesRenderer({
            native, owned: native && session.client.owns_window(window),
            pid: window.get_pid(), expectedPid: session.pid,
            title: window.get_title(), appId: window.get_wm_class(), token: session.token,
        });
        if (!matches) {
            if (session.window === window)
                this._stop(session, 'Owned renderer window identity changed', true);
            return;
        }
        if (session.window && session.window !== window) {
            this._stop(session, 'Renderer created more than one matching window', true);
            return;
        }
        if (!session.window) {
            if (session.candidates.get(window).shown)
                throw new Error('Renderer identity was not ready before first shown');
            const actor = window.get_compositor_private();
            if (!actor)
                return;
            session.window = window;
            session.actor = actor;
            window.set_type(Meta.WindowType.UTILITY);
            window.hide_from_window_list();
            actor.opacity = 0;
            this._connect(session, actor, 'destroy', () =>
                this._stop(session, 'Owned compositor actor was destroyed', true));
        }
        if (session.candidates.get(window).shown && !session.shown) {
            // Before first shown, inhibition prevents the initial Wayland frame callbacks.
            window.inhibit_mapped();
            session.inhibited = true;
            session.shown = true;
            this._attachClone(session);
            session.overviewClones = new OverviewClones(
                session.actor, session.monitor, session.monitorIndex,
                error => this._stop(session, `Overview integration failed: ${error.message}`, true));
            session.overviewClones.start();
            this._updateVisibility(session);
            if (!this._live(session))
                return;
            session.state = 'running';
            session.detail = 'Renderer running on the selected desktop output';
            this._clearTimer(session, 'readyTimer');
        }
    }

    _attachClone(session) {
        if (!session.shown || !this._live(session))
            return;
        session.clone?.destroy();
        const background = session.manager.backgroundActor;
        if (!background)
            throw new Error('Selected desktop background disappeared');
        const {logicalWidth: width, logicalHeight: height} = session.geometry;
        const clone = new Clutter.Clone({
            source: session.actor, reactive: false, x: 0, y: 0, width, height, visible: false,
        });
        session.clone = clone;
        clone.connect('destroy', () => {
            if (session.clone === clone)
                session.clone = null;
        });
        clone.set_clip(0, 0, width, height);
        // Keep the whole source actor parented; mapped clones drive Wayland updates.
        background.add_child(clone);
        this._updateVisibility(session);
    }

    _updateVisibility(session) {
        const allowed = this._live(session) && Main.sessionMode.hasWindows &&
            !Main.sessionMode.isLocked && !Main.sessionMode.isGreeter &&
            !Main.layoutManager.screenShieldGroup.visible;
        if (session.clone)
            session.clone.visible = allowed && !session.overview;
        session.overviewClones?.setVisibility(allowed, session.overview);
    }

    _timer(milliseconds, callback) {
        return GLib.timeout_add(GLib.PRIORITY_DEFAULT, milliseconds, () => {
            callback();
            return GLib.SOURCE_REMOVE;
        });
    }

    _clearTimer(session, key) {
        if (session[key])
            GLib.source_remove(session[key]);
        session[key] = 0;
    }

    _warning(session, message) {
        if (!session.warnings.has(message)) {
            session.warnings.add(message);
            console.error(`Wallbreak: ${message}`);
        }
    }

    _cleanupVisuals(session) {
        if (session.overviewClones) {
            try {
                session.overviewClones.destroy();
                session.overviewClones = null;
            } catch (error) {
                this._warning(session, `Overview cleanup failed: ${error.message}`);
            }
        }
        session.signals = session.signals.filter(([object, id]) => {
            try {
                object.disconnect(id);
                return false;
            } catch (error) {
                this._warning(session, `Signal cleanup failed: ${error.message}`);
                return true;
            }
        });
        try {
            session.clone?.destroy();
            session.clone = null;
        } catch (error) {
            this._warning(session, `Clone cleanup failed: ${error.message}`);
        }
        if (session.inhibited) {
            try {
                session.window.uninhibit_mapped();
                session.inhibited = false;
            } catch (error) {
                this._warning(session, `Mapping inhibitor cleanup failed: ${error.message}`);
            }
        }
        // Never restore opacity or type while the child could still show a window.
    }

    _stop(session, detail, failed = false) {
        if (!this._current(session) || session.settled)
            return;
        if (failed)
            session.failure ??= detail;
        if (session.state !== 'stopping') {
            session.state = 'stopping';
            session.detail = detail;
            this._clearTimer(session, 'readyTimer');
            this._cleanupVisuals(session);
            if (session.process && !session.reaped) {
                try {
                    session.process.send_signal(15);
                } catch (error) {
                    this._warning(session, `SIGTERM failed: ${error.message}`);
                }
                session.killTimer = this._timer(TERM_MS, () => {
                    session.killTimer = 0;
                    if (session.reaped || !this._current(session))
                        return;
                    const warning = 'Forced cleanup: renderer did not exit within 3 seconds of SIGTERM; sending SIGKILL';
                    session.failure ??= warning;
                    session.forced = true;
                    this._warning(session, warning);
                    try {
                        session.process.force_exit();
                    } catch (error) {
                        this._warning(session, `SIGKILL failed: ${error.message}`);
                    }
                });
            }
        }
        this._finish(session);
    }

    _wait(session) {
        // A failed wait is not proof of exit. Retain ownership and retry.
        const failed = error => {
            if (!this._current(session) || session.settled || session.reaped)
                return;
            this._stop(session, `Renderer wait failed: ${error.message}`, true);
            session.waitTimer = this._timer(1_000, () => {
                session.waitTimer = 0;
                this._wait(session);
            });
        };
        try {
            session.process.wait_async(null, (process, result) => {
                try {
                    process.wait_finish(result);
                } catch (error) {
                    failed(error);
                    return;
                }
                if (!this._current(session) || session.settled)
                    return;
                session.reaped = true;
                this._clearTimer(session, 'killTimer');
                session.exitDetail = process.get_if_exited() ?
                    `Renderer exited with status ${process.get_exit_status()}` :
                    `Renderer terminated by signal ${process.get_term_sig()}`;
                if (process.get_if_exited() ? process.get_exit_status() !== 0 : process.get_term_sig() !== 15)
                    session.failure ??= session.exitDetail;
                if (session.state !== 'stopping')
                    this._stop(session, session.exitDetail, true);
                if (!session.stderrDone) {
                    session.drainTimer = this._timer(DRAIN_MS, () => {
                        session.drainTimer = 0;
                        this._warning(session, 'Stderr drain truncated: pipe remained open after renderer exit');
                        session.readCancel.cancel();
                    });
                }
                this._finish(session);
            });
        } catch (error) {
            failed(error);
        }
    }

    _readStderr(session) {
        const failed = error => {
            if (!session.readCancel.is_cancelled()) {
                this._warning(session, `Stderr read failed: ${error.message}`);
                this._stop(session, 'Renderer stderr could not be read', true);
            }
            this._closeStderr(session);
        };
        try {
            session.stderr.read_bytes_async(4096, GLib.PRIORITY_DEFAULT, session.readCancel, (stream, result) => {
                let bytes;
                try {
                    bytes = stream.read_bytes_finish(result).get_data();
                } catch (error) {
                    failed(error);
                    return;
                }
                if (bytes.length > 0) {
                    session.tail.append(bytes);
                    this._readStderr(session);
                } else {
                    this._closeStderr(session);
                }
            });
        } catch (error) {
            failed(error);
        }
    }

    _closeStderr(session) {
        if (session.stderrDone || session.closingStderr)
            return;
        session.closingStderr = true;
        const finished = error => {
            session.closingStderr = false;
            if (error) {
                this._warning(session, `Stderr close failed: ${error.message}`);
                this._stop(session, 'Renderer stderr cleanup failed', true);
                if (!session.stderr.is_closed()) {
                    session.closeTimer = this._timer(1_000, () => {
                        session.closeTimer = 0;
                        this._closeStderr(session);
                    });
                    return;
                }
            }
            session.stderrDone = true;
            this._clearTimer(session, 'drainTimer');
            this._finish(session);
        };
        try {
            session.stderr.close_async(GLib.PRIORITY_DEFAULT, null, (input, result) => {
                try {
                    input.close_finish(result);
                } catch (error) {
                    finished(error);
                    return;
                }
                finished();
            });
        } catch (error) {
            finished(error);
        }
    }

    _unwatchOwner(session) {
        if (session.ownerWatch)
            Gio.bus_unwatch_name(session.ownerWatch);
        session.ownerWatch = 0;
    }

    _detail(session) {
        return [session.failure ?? session.detail, session.exitDetail,
            ...session.warnings, session.tail.text()].filter(Boolean).join('\n').replaceAll('\0', '\\0');
    }

    _finish(session) {
        if (session.settled || !session.reaped || !session.stderrDone)
            return;
        this._cleanupVisuals(session);
        if (session.inhibited || session.signals.length > 0 || session.clone ||
            session.overviewClones?.hasResources) {
            session.failure ??= 'Shell resource cleanup is incomplete';
            if (!session.cleanupTimer) {
                session.cleanupTimer = this._timer(1_000, () => {
                    session.cleanupTimer = 0;
                    this._finish(session);
                });
            }
            return;
        }
        for (const key of ['readyTimer', 'killTimer', 'drainTimer', 'waitTimer', 'closeTimer', 'cleanupTimer'])
            this._clearTimer(session, key);
        session.state = session.failure ? 'failed' : 'stopped';
        session.settled = true;
        session.process = null;
        session.client = null;
        session.stderr = null;
        session.readCancel = null;
        session.candidates.clear();
        session.pendingWindows = [];
        session.window = null;
        session.actor = null;
        session.manager = null;
        session.monitor = null;
        session.overviewClones = null;
        if (session.ownerGone || session.discardFinal || !this._enabled) {
            this._unwatchOwner(session);
            if (this._current(session))
                this._session = null;
        }
    }
}
