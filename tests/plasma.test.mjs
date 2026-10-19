// Run: node --test tests/plasma.test.mjs
// Plasma scripting boundaries are simulated, not a live plasmashell session.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const source = await readFile(new URL('../src/plasma.js', import.meta.url), 'utf8');
const imagePlugin = 'org.kde.image';
const defaults = {Image: '', FillMode: 2, Color: '#ffffff', Blur: false};
const applied = {Image: 'file:///playback.png', FillMode: 2, Color: '#000000', Blur: false};
const imageGroup = ['Wallpaper', imagePlugin, 'General'];
const ownerGroup = ['Wallbreak'];
const definitions = [
    {id: 11, screen: 0, plugin: 'org.kde.slideshow',
        settings: {Image: 'file:///left.png', FillMode: 0, Color: '#112233', Blur: true,
            Unmanaged: 'left setting'}},
    {id: 27, screen: 1, plugin: imagePlugin,
        settings: {Image: 'file:///right.png', FillMode: 1, Color: '#445566', Blur: false,
            Unmanaged: 'right setting'}},
];

function harness(input = definitions) {
    const desktops = new Map(input.map(definition => [definition.id, {
        activity: 'current-activity', screen: 0, plugin: imagePlugin, owner: '', ...definition,
        settings: {...definition.settings},
    }]));
    const h = {desktops, mutations: [], commits: [], reads: [], lookups: [], activities: []};
    h.settings = id => Object.fromEntries(Object.keys(defaults).map(key => [key,
        Object.hasOwn(desktops.get(id).settings, key)
            ? desktops.get(id).settings[key] : defaults[key]]));
    h.state = () => structuredClone([...desktops.values()]);
    h.run = (request, {commitPlugins = true} = {}) => {
        request = {owner: ':1.42', partial: false, ...request};
        const wrappers = new Map();
        const output = [];
        const wrap = desktop => {
            if (!desktop)
                return undefined;
            if (wrappers.has(desktop.id))
                return wrappers.get(desktop.id).view;
            // Plasma caches the setter/getter on this invocation's wrapper;
            // changing it does not change the containment's real plugin yet.
            const cached = {plugin: desktop.plugin, dirty: false};
            const view = {
                id: desktop.id, screen: desktop.screen, currentConfigGroup: [],
                get wallpaperPlugin() { return cached.plugin; },
                set wallpaperPlugin(value) {
                    h.mutations.push(['plugin', desktop.id, value]);
                    cached.plugin = value;
                    cached.dirty = true;
                },
                readConfig(key, fallback) {
                    h.reads.push({id: desktop.id, key, fallback});
                    const group = Array.from(this.currentConfigGroup);
                    if (group.length === 1) {
                        assert.deepEqual(group, ownerGroup);
                        assert.equal(key, 'Owner');
                        assert.equal(fallback, '');
                        return desktop.owner;
                    }
                    assert.deepEqual(group, imageGroup);
                    return Object.hasOwn(desktop.settings, key) ? desktop.settings[key] : fallback;
                },
                writeConfig(key, value) {
                    const group = Array.from(this.currentConfigGroup);
                    if (group.length === 1) {
                        assert.deepEqual(group, ownerGroup);
                        assert.equal(key, 'Owner');
                        h.mutations.push(['owner', desktop.id, value]);
                        if (h.onWrite?.(desktop, key, value) !== false)
                            desktop.owner = value;
                        return;
                    }
                    assert.deepEqual(group, imageGroup);
                    assert.equal(desktop.plugin, imagePlugin,
                        'image writes require a plugin committed by an earlier invocation');
                    h.mutations.push(['write', desktop.id, key, value]);
                    if (h.onWrite?.(desktop, key, value) !== false)
                        desktop.settings[key] = value;
                },
            };
            wrappers.set(desktop.id, {view, cached, desktop});
            return view;
        };
        const context = {
            request: JSON.parse(JSON.stringify(request)),
            __plasmaInjected: false,
            currentActivity: () => {
                h.activities.push('currentActivity');
                return 'current-activity';
            },
            desktopsForActivity: activity => {
                h.activities.push(activity);
                return [...desktops.values()].filter(desktop => desktop.activity === activity)
                    .map(wrap);
            },
            desktopById: id => {
                h.lookups.push(id);
                return wrap(desktops.get(id));
            },
            print: value => output.push(value),
        };
        h.output = output;
        try {
            vm.runInNewContext(source, context, {filename: 'src/plasma.js', timeout: 1000});
        } finally {
            // Wrapper destruction commits even when evaluation throws. The
            // disabled-commit case models a setter Plasma failed to retain.
            for (const {desktop, cached} of wrappers.values()) {
                if (cached.dirty && commitPlugins) {
                    h.commits.push([desktop.id, desktop.plugin, cached.plugin]);
                    desktop.plugin = cached.plugin;
                }
            }
            assert.equal(context.__plasmaInjected, false, 'request strings must not execute');
        }
        if (request.action === 'snapshot') {
            assert.equal(output.length, 1, 'snapshot must print exactly one JSON result');
            return JSON.parse(output[0]);
        }
        assert.deepEqual(output, ['null'], 'operations must print one null acknowledgement');
        return null;
    };
    return h;
}

test('snapshot selects only connected desktops in the current activity', () => {
    const h = harness([
        definitions[0],
        {id: 30, screen: -1, settings: {Image: 'file:///disconnected.png'}},
        {...definitions[1], screen: 3},
        {id: 91, screen: 0, activity: 'other-activity', settings: {Image: 'file:///other.png'}},
    ]);
    const before = h.state();
    assert.deepEqual(h.run({action: 'snapshot'}), [
        {id: 11, plugin: 'org.kde.slideshow', settings: {
            Image: 'file:///left.png', FillMode: 0, Color: '#112233', Blur: true}},
        {id: 27, plugin: imagePlugin, settings: {
            Image: 'file:///right.png', FillMode: 1, Color: '#445566', Blur: false}},
    ]);
    assert.deepEqual(h.activities, ['currentActivity', 'current-activity']);
    assert.deepEqual(h.lookups, []);
    assert.deepEqual(h.reads.map(({id}) => id), [11, 11, 11, 11, 27, 27, 27, 27]);
    assert.deepEqual(h.mutations, []);
    assert.deepEqual(h.state(), before);
});

test('snapshot uses the exact defaults without replacing explicit false, zero or empty values', () => {
    const explicit = {Image: '', FillMode: 0, Color: '', Blur: false};
    const h = harness([{id: 1}, {id: 2, settings: explicit}]);
    assert.deepEqual(h.run({action: 'snapshot'}), [
        {id: 1, plugin: imagePlugin, settings: defaults},
        {id: 2, plugin: imagePlugin, settings: explicit},
    ]);
    assert.deepEqual(h.reads, [1, 2].flatMap(id => Object.entries(defaults)
        .map(([key, fallback]) => ({id, key, fallback}))));
});

test('snapshot rejects an activity without any connected desktop', async t => {
    for (const [name, input] of [
        ['no containments', []],
        ['disconnected containment', [{id: 1, screen: -1}]],
        ['other activity only', [{id: 1, activity: 'other-activity'}]],
    ]) {
        await t.test(name, () => {
            const h = harness(input);
            assert.throws(() => h.run({action: 'snapshot'}), /no active desktop containments/);
            assert.deepEqual(h.output, []);
            assert.deepEqual(h.mutations, []);
        });
    }
});

test('switch prevalidates every desktop before setting any plugin or configuration', async t => {
    const changes = {
        plugin: desktop => { desktop.plugin = 'org.kde.user-wallpaper'; },
        Image: desktop => { desktop.settings.Image = 'file:///user.png'; },
        FillMode: desktop => { desktop.settings.FillMode = 0; },
        Color: desktop => { desktop.settings.Color = '#abcdef'; },
        Blur: desktop => { desktop.settings.Blur = true; },
        removed: (_desktop, h) => h.desktops.delete(27),
    };
    for (const [name, change] of Object.entries(changes)) {
        await t.test(name, () => {
            const h = harness();
            const previous = h.run({action: 'snapshot'});
            change(h.desktops.get(27), h);
            const before = h.state();
            assert.throws(() => h.run({action: 'switch', previous, applied, partial: true}),
                /wallpaper changed before playback/);
            assert.deepEqual(h.lookups, [11, 27]);
            assert.deepEqual(h.mutations, []);
            assert.deepEqual(h.commits, []);
            assert.deepEqual(h.output, []);
            assert.deepEqual(h.state(), before);
        });
    }
});

test('switch commits plugins at teardown without writing image settings', () => {
    const h = harness();
    const previous = h.run({action: 'snapshot'});
    const before = h.state();
    h.run({action: 'switch', previous, applied});
    assert.deepEqual(h.mutations, [
        ['owner', 11, ':1.42'], ['plugin', 11, imagePlugin],
        ['owner', 27, ':1.42'], ['plugin', 27, imagePlugin],
    ]);
    assert.deepEqual(h.commits, [
        [11, 'org.kde.slideshow', imagePlugin], [27, imagePlugin, imagePlugin],
    ]);
    assert.deepEqual(h.state(), before.map(desktop => ({
        ...desktop, plugin: imagePlugin, owner: ':1.42',
    })));
});

test('apply requires a real plugin switch committed in a preceding invocation', () => {
    const h = harness();
    const previous = h.run({action: 'snapshot'});
    assert.throws(() => h.run({action: 'apply', previous, applied, partial: true}), /did not retain/);
    assert.deepEqual(h.mutations, []);

    h.run({action: 'switch', previous, applied, partial: true}, {commitPlugins: false});
    assert.equal(h.desktops.get(11).plugin, 'org.kde.slideshow');
    h.mutations.length = 0;
    assert.throws(() => h.run({action: 'apply', previous, applied, partial: true}), /did not retain/);
    assert.deepEqual(h.mutations, []);

    h.run({action: 'switch', previous, applied});
    h.mutations.length = 0;
    h.run({action: 'apply', previous, applied});
    assert.deepEqual(h.mutations, previous.flatMap(({id}) => Object.entries(applied)
        .map(([key, value]) => ['write', id, key, value])));
    for (const {id} of previous)
        assert.deepEqual(h.settings(id), applied);
});

test('apply prevalidates all owners, plugins and original settings before its first write', async t => {
    const changes = {
        'missing owner': desktop => { desktop.owner = ''; },
        'foreign owner': desktop => { desktop.owner = ':1.99'; },
        plugin: desktop => { desktop.plugin = 'org.kde.user-wallpaper'; },
        Image: desktop => { desktop.settings.Image = 'file:///user.png'; },
        FillMode: desktop => { desktop.settings.FillMode = 0; },
        Color: desktop => { desktop.settings.Color = '#abcdef'; },
        Blur: desktop => { desktop.settings.Blur = true; },
        removed: (_desktop, h) => h.desktops.delete(27),
    };
    for (const [name, change] of Object.entries(changes)) {
        await t.test(name, () => {
            const h = harness();
            const previous = h.run({action: 'snapshot'});
            h.run({action: 'switch', previous, applied});
            change(h.desktops.get(27), h);
            const before = h.state();
            h.mutations.length = 0;
            assert.throws(() => h.run({action: 'apply', previous, applied, partial: true}),
                /did not retain/);
            assert.deepEqual(h.mutations, []);
            assert.deepEqual(h.output, []);
            assert.deepEqual(h.state(), before);
        });
    }
});

test('verify reads every applied setting without making mutations', () => {
    const h = harness();
    const previous = h.run({action: 'snapshot'});
    h.run({action: 'switch', previous, applied});
    h.run({action: 'apply', previous, applied});
    const before = h.state();
    h.mutations.length = 0;
    h.reads.length = 0;
    h.run({action: 'verify', previous, applied});
    assert.deepEqual(h.reads.map(({id, key}) => [id, key]),
        previous.flatMap(({id}) => ['Owner', ...Object.keys(defaults)].map(key => [id, key])));
    assert.deepEqual(h.mutations, []);
    assert.deepEqual(h.state(), before);
});

test('verify detects lost ownership, changed plugins and strict value mismatches', async t => {
    const changes = {
        'missing owner': desktop => { desktop.owner = ''; },
        'foreign owner': desktop => { desktop.owner = ':1.99'; },
        plugin: desktop => { desktop.plugin = 'org.kde.user-wallpaper'; },
        Image: desktop => { desktop.settings.Image = 'file:///user.png'; },
        FillMode: desktop => { desktop.settings.FillMode = String(applied.FillMode); },
        Color: desktop => { desktop.settings.Color = '#abcdef'; },
        Blur: desktop => { desktop.settings.Blur = true; },
        removed: (_desktop, h) => h.desktops.delete(27),
    };
    for (const [name, change] of Object.entries(changes)) {
        await t.test(name, () => {
            const h = harness();
            const previous = h.run({action: 'snapshot'});
            h.run({action: 'switch', previous, applied});
            h.run({action: 'apply', previous, applied});
            change(h.desktops.get(27), h);
            const before = h.state();
            h.mutations.length = 0;
            assert.throws(() => h.run({action: 'verify', previous, applied}), /did not retain/);
            assert.deepEqual(h.mutations, []);
            assert.deepEqual(h.output, []);
            assert.deepEqual(h.state(), before);
        });
    }
});

test('apply rejects silently lost image or FillMode writes before ack and permits partial rollback', async t => {
    for (const key of ['Image', 'FillMode']) {
        await t.test(key, () => {
            const h = harness();
            const before = h.state();
            const previous = h.run({action: 'snapshot'});
            h.run({action: 'switch', previous, applied, partial: true});
            h.onWrite = (desktop, writtenKey) => !(desktop.id === 27 && writtenKey === key);
            assert.throws(() => h.run({action: 'apply', previous, applied, partial: true}),
                /Plasma did not retain the requested wallpaper settings/);
            assert.deepEqual(h.output, [], 'failed readback must not acknowledge apply');
            assert.deepEqual(h.settings(11), applied);
            assert.deepEqual(h.settings(27), {...applied, [key]: previous[1].settings[key]});
            for (const desktop of h.desktops.values()) {
                assert.equal(desktop.plugin, imagePlugin);
                assert.equal(desktop.owner, ':1.42');
            }
            h.onWrite = undefined;
            h.run({action: 'restore', previous, applied, partial: true});
            assert.deepEqual(h.state(), before, 'rollback restores every original config and plugin');
        });
    }
});

test('restore independently restores each original plugin and image settings', () => {
    const h = harness();
    const before = h.state();
    const previous = h.run({action: 'snapshot'});
    h.run({action: 'switch', previous, applied});
    h.run({action: 'apply', previous, applied});
    h.run({action: 'verify', previous, applied});
    h.run({action: 'restore', previous, applied});
    assert.deepEqual(h.state(), before);
    assert.deepEqual(previous.map(({plugin, settings}) => ({plugin, settings})),
        before.map(desktop => ({plugin: desktop.plugin, settings: h.settings(desktop.id)})));
    h.run({action: 'restore', previous, applied});
    assert.deepEqual(h.state(), before, 'repeated cleanup must not change the restored state');
});

test('restore reports a silently lost write without clearing ownership or switching plugins', () => {
    const h = harness();
    const before = h.state();
    const previous = h.run({action: 'snapshot'});
    h.run({action: 'switch', previous, applied});
    h.run({action: 'apply', previous, applied});
    const untouchedDesktop = structuredClone(h.desktops.get(27));
    h.mutations.length = 0;
    h.commits.length = 0;
    h.onWrite = (desktop, key) => !(desktop.id === 11 && key === 'FillMode');
    assert.throws(() => h.run({action: 'restore', previous, applied}),
        /Plasma could not restore the previous wallpaper settings/);
    assert.deepEqual(h.output, [], 'failed restoration must not acknowledge success');
    assert.deepEqual(h.desktops.get(11), {
        ...before[0], plugin: imagePlugin, owner: ':1.42',
        settings: {...before[0].settings, FillMode: applied.FillMode},
    });
    assert.deepEqual(h.mutations, Object.entries(previous[0].settings)
        .map(([key, value]) => ['write', 11, key, value]),
    'failed readback must not clear the marker or set the original plugin');
    assert.deepEqual(h.commits, []);
    assert.deepEqual(h.desktops.get(27), untouchedDesktop);
});

test('restore rolls back a switch even when apply never ran', () => {
    const h = harness();
    const before = h.state();
    const previous = h.run({action: 'snapshot'});
    h.run({action: 'switch', previous, applied, partial: true});
    h.run({action: 'restore', previous, applied, partial: true});
    assert.deepEqual(h.state(), before);
});

test('restore preserves a user changed image, plugin or setting and still restores other desktops', async t => {
    const changes = {
        plugin: desktop => { desktop.plugin = 'org.kde.user-wallpaper'; },
        Image: desktop => { desktop.settings.Image = 'file:///user.png'; },
        FillMode: desktop => { desktop.settings.FillMode = 0; },
        Color: desktop => { desktop.settings.Color = '#abcdef'; },
        Blur: desktop => { desktop.settings.Blur = true; },
    };
    for (const [name, change] of Object.entries(changes)) {
        await t.test(name, () => {
            const h = harness();
            const before = h.state();
            const previous = h.run({action: 'snapshot'});
            h.run({action: 'switch', previous, applied});
            h.run({action: 'apply', previous, applied});
            change(h.desktops.get(27));
            const userDesktop = structuredClone(h.desktops.get(27));
            h.mutations.length = 0;
            h.run({action: 'restore', previous, applied});
            assert.deepEqual(h.desktops.get(11), before[0]);
            assert.deepEqual(h.desktops.get(27), {...userDesktop, owner: ''});
            assert.deepEqual(h.mutations.filter(([, id]) => id === 27), [['owner', 27, '']],
                'release ownership without rewriting user wallpaper values');
        });
    }
});

test('restore rolls back a partial apply failure at any desktop or setting', async t => {
    for (const id of [11, 27]) {
        for (const key of Object.keys(defaults)) {
            await t.test(`${id} ${key}`, () => {
                const h = harness();
                const before = h.state();
                const previous = h.run({action: 'snapshot'});
                h.run({action: 'switch', previous, applied, partial: true});
                h.onWrite = (desktop, writtenKey) => {
                    if (desktop.id === id && writtenKey === key)
                        throw new Error('simulated write failure');
                };
                assert.throws(() => h.run({action: 'apply', previous, applied, partial: true}),
                    /simulated write failure/);
                assert.deepEqual(h.output, []);
                if (id === 27)
                    assert.deepEqual(h.settings(11), applied, 'earlier desktops were already written');
                const stopped = {...previous.find(desktop => desktop.id === id).settings};
                for (const writtenKey of Object.keys(defaults)) {
                    if (writtenKey === key)
                        break;
                    stopped[writtenKey] = applied[writtenKey];
                }
                assert.deepEqual(h.settings(id), stopped, 'failure leaves only preceding writes');
                h.onWrite = undefined;
                h.run({action: 'restore', previous, applied, partial: true});
                assert.deepEqual(h.state(), before);
            });
        }
    }
});

test('restore skips a removed desktop without preventing cleanup of surviving desktops', () => {
    const h = harness();
    const before = h.state();
    const previous = h.run({action: 'snapshot'});
    h.run({action: 'switch', previous, applied});
    h.run({action: 'apply', previous, applied});
    h.desktops.delete(11);
    h.mutations.length = 0;
    h.run({action: 'restore', previous, applied});
    assert.deepEqual(h.state(), [before[1]]);
    assert.ok(h.mutations.every(([, id]) => id === 27));
});

test('hostile image and plugin strings remain data throughout the lifecycle', () => {
    const hostile = '\"; globalThis.__plasmaInjected = true; print("injected"); //\n' +
        '\\${notCode}$(touch nope)\u2028\u2029';
    const h = harness([
        {id: 1, plugin: '__proto__', settings: {...defaults, Image: `file:///${hostile}`}},
        {id: 2, plugin: hostile, settings: {...defaults, Color: hostile}},
    ]);
    const before = h.state();
    const previous = h.run({action: 'snapshot'});
    assert.equal(previous[0].plugin, '__proto__');
    assert.equal(previous[0].settings.Image, `file:///${hostile}`);
    assert.equal(previous[1].plugin, hostile);
    const requested = JSON.parse(JSON.stringify({...applied, Image: `file:///applied-${hostile}`})
        .replace(/}$/, ',"__proto__":{"__plasmaInjected":true}}'));
    for (const action of ['switch', 'apply', 'verify'])
        h.run({action, previous, applied: requested, owner: hostile});
    for (const {id} of previous) {
        assert.deepEqual(h.settings(id), {...applied, Image: requested.Image});
        assert.equal(h.desktops.get(id).owner, hostile);
    }
    h.run({action: 'restore', previous, applied: requested, owner: hostile});
    assert.deepEqual(h.state(), before);
    assert.equal({}.__plasmaInjected, undefined);
});

test('unknown actions, including executable-looking strings, do not mutate desktops', () => {
    const h = harness();
    const previous = h.run({action: 'snapshot'});
    const before = h.state();
    assert.throws(() => h.run({
        action: 'restore; globalThis.__plasmaInjected = true;', previous, applied,
    }), /Unknown wallbreak Plasma operation/);
    assert.deepEqual(h.mutations, []);
    assert.deepEqual(h.output, []);
    assert.deepEqual(h.state(), before);
});

test('restore preserves user changes back to an original setting after a successful apply', () => {
    const h = harness();
    const previous = h.run({action: 'snapshot'});
    h.run({action: 'switch', previous, applied});
    h.run({action: 'apply', previous, applied});
    h.desktops.get(27).settings.FillMode = previous[1].settings.FillMode;
    const userDesktop = structuredClone(h.desktops.get(27));
    h.mutations.length = 0;
    h.run({action: 'restore', previous, applied});
    assert.deepEqual(h.desktops.get(27), {...userDesktop, owner: ''},
        'a user-selected old FillMode does not authorize restoring the other applied settings');
    assert.deepEqual(h.mutations.filter(([, id]) => id === 27), [['owner', 27, '']]);
});

test('rollback after rejected switch prevalidation preserves a user-selected image plugin', () => {
    const h = harness();
    const previous = h.run({action: 'snapshot'});
    h.desktops.get(11).plugin = imagePlugin;
    const userDesktop = structuredClone(h.desktops.get(11));
    assert.throws(() => h.run({action: 'switch', previous, applied, partial: true}),
        /changed before playback/);
    assert.deepEqual(h.mutations, [], 'prevalidation rejected the switch without taking ownership');
    h.run({action: 'restore', previous, applied, partial: true});
    assert.deepEqual(h.desktops.get(11), userDesktop,
        'cleanup must not undo a plugin choice made before wallbreak switched anything');
    assert.deepEqual(h.mutations, [], 'rejected prevalidation leaves no owned desktops to clean up');
});

test('a different request owner cannot apply, verify or restore another client\'s desktops', async t => {
    for (const action of ['apply', 'verify', 'restore']) {
        await t.test(action, () => {
            const h = harness();
            const previous = h.run({action: 'snapshot'});
            h.run({action: 'switch', previous, applied});
            if (action !== 'apply')
                h.run({action: 'apply', previous, applied});
            const before = h.state();
            h.mutations.length = 0;
            const request = {action, previous, applied, owner: ':1.99', partial: action === 'apply'};
            if (action === 'restore')
                h.run(request);
            else
                assert.throws(() => h.run(request), /did not retain/);
            assert.deepEqual(h.mutations, []);
            assert.deepEqual(h.state(), before, 'foreign ownership markers must not be cleared');
        });
    }
});

test('restore skips desktops with absent or foreign ownership in both cleanup modes', async t => {
    for (const partial of [false, true]) {
        for (const owner of ['', ':1.99']) {
            await t.test(`${partial ? 'partial' : 'normal'} ${owner || 'missing owner'}`, () => {
                const h = harness();
                const original = h.state();
                const previous = h.run({action: 'snapshot'});
                h.run({action: 'switch', previous, applied});
                h.run({action: 'apply', previous, applied});
                h.desktops.get(27).owner = owner;
                const unownedDesktop = structuredClone(h.desktops.get(27));
                h.mutations.length = 0;
                h.run({action: 'restore', previous, applied, partial});
                assert.deepEqual(h.desktops.get(11), original[0]);
                assert.deepEqual(h.desktops.get(27), unownedDesktop);
                assert.ok(h.mutations.every(([, id]) => id !== 27));
            });
        }
    }
});

test('partial cleanup restores only markers claimed before a switch failure', () => {
    const h = harness();
    const before = h.state();
    const previous = h.run({action: 'snapshot'});
    h.onWrite = (desktop, key) => {
        if (desktop.id === 27 && key === 'Owner')
            throw new Error('simulated ownership write failure');
    };
    assert.throws(() => h.run({action: 'switch', previous, applied, partial: true}),
        /simulated ownership write failure/);
    assert.equal(h.desktops.get(11).plugin, imagePlugin);
    assert.equal(h.desktops.get(11).owner, ':1.42');
    assert.deepEqual(h.desktops.get(27), before[1]);
    h.onWrite = undefined;
    h.mutations.length = 0;
    h.run({action: 'restore', previous, applied, partial: true});
    assert.deepEqual(h.state(), before);
    assert.ok(h.mutations.every(([, id]) => id === 11));
});
