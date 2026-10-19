// Rust-owned Plasma API fixture. Only the API/wrapper behavior is mocked;
// every operation evaluates the production script, including its JSON.parse prefix.
use super::*;
use rquickjs::{
    Array, CatchResultExt, Context as JsContext, Ctx, Exception, Function, Object, Runtime,
    Value as JsValue, function::This, object::Accessor,
};
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};

const IMAGE_PLUGIN: &str = "org.kde.image";
const OWNER: &str = ":1.42";
const KEYS: [&str; 4] = ["Image", "FillMode", "Color", "Blur"];

fn defaults() -> Value {
    json!({"Image": "", "FillMode": 2, "Color": "#ffffff", "Blur": false})
}

fn applied() -> Value {
    json!({"Image": "file:///playback.png", "FillMode": 2, "Color": "#000000", "Blur": false})
}

fn definitions() -> Value {
    json!([
        {"id": 11, "screen": 0, "plugin": "org.kde.slideshow", "settings": {
            "Image": "file:///left.png", "FillMode": 0, "Color": "#112233", "Blur": true,
            "Unmanaged": "left setting"
        }},
        {"id": 27, "screen": 1, "plugin": IMAGE_PLUGIN, "settings": {
            "Image": "file:///right.png", "FillMode": 1, "Color": "#445566", "Blur": false,
            "Unmanaged": "right setting"
        }}
    ])
}

fn request(action: &str, previous: &Value) -> Value {
    json!({"action": action, "previous": previous, "applied": applied()})
}

#[derive(Clone, Debug, PartialEq)]
struct MockDesktop {
    id: u32,
    screen: i64,
    activity: String,
    plugin: String,
    owner: String,
    settings: Value,
}

#[derive(Clone, Copy)]
struct WriteFault {
    id: u32,
    key: &'static str,
    // None models a successful API call whose write is silently discarded.
    error: Option<&'static str>,
}

#[derive(Default)]
struct State {
    desktops: Vec<MockDesktop>,
    mutations: Vec<Value>,
    commits: Vec<Value>,
    reads: Vec<Value>,
    lookups: Vec<u32>,
    activities: Vec<String>,
    output: Vec<String>,
    fault: Option<WriteFault>,
}

impl State {
    fn desktop(&self, id: u32) -> &MockDesktop {
        self.desktops
            .iter()
            .find(|desktop| desktop.id == id)
            .unwrap()
    }

    fn desktop_mut(&mut self, id: u32) -> &mut MockDesktop {
        self.desktops
            .iter_mut()
            .find(|desktop| desktop.id == id)
            .unwrap()
    }
}

struct CachedPlugin {
    plugin: String,
    dirty: bool,
}

struct Harness {
    state: Rc<RefCell<State>>,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new(definitions())
    }
}

// Conversion is only at the mocked API boundary, not an implementation of any
// wallpaper operation. JSON retains strings, numbers and booleans distinctly.
fn rust_value<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Value {
    let text = ctx
        .json_stringify(value)
        .unwrap()
        .unwrap()
        .to_string()
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn wrapper<'js>(
    ctx: &Ctx<'js>,
    state: Rc<RefCell<State>>,
    desktop: &MockDesktop,
    cache: Rc<RefCell<CachedPlugin>>,
) -> Object<'js> {
    let id = desktop.id;
    let view = Object::new(ctx.clone()).unwrap();
    view.set("id", id).unwrap();
    view.set("screen", desktop.screen).unwrap();
    view.set("currentConfigGroup", Array::new(ctx.clone()).unwrap())
        .unwrap();
    let getter = cache.clone();
    let setter_state = state.clone();
    view.prop(
        "wallpaperPlugin",
        Accessor::new(
            move || getter.borrow().plugin.clone(),
            move |plugin: String| {
                setter_state
                    .borrow_mut()
                    .mutations
                    .push(json!(["plugin", id, plugin]));
                let mut cache = cache.borrow_mut();
                cache.plugin = plugin;
                cache.dirty = true;
            },
        ),
    )
    .unwrap();
    let read_state = state.clone();
    view.set(
        "readConfig",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, this: This<Object<'js>>, key: String, fallback: JsValue<'js>| {
                let group: Vec<String> = this.0.get("currentConfigGroup")?;
                let fallback = rust_value(&ctx, fallback);
                let mut state = read_state.borrow_mut();
                state
                    .reads
                    .push(json!({"id": id, "key": key, "fallback": fallback}));
                let desktop = state.desktop(id);
                let value = if group.len() == 1 {
                    assert_eq!(group, ["Wallbreak"]);
                    assert_eq!(key, "Owner");
                    assert_eq!(fallback, json!(""));
                    json!(desktop.owner)
                } else {
                    assert_eq!(group, ["Wallpaper", IMAGE_PLUGIN, "General"]);
                    desktop.settings.get(&key).cloned().unwrap_or(fallback)
                };
                ctx.json_parse(value.to_string())
            },
        )
        .unwrap(),
    )
    .unwrap();
    view.set(
        "writeConfig",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, this: This<Object<'js>>, key: String, value: JsValue<'js>| {
                let group: Vec<String> = this.0.get("currentConfigGroup")?;
                let value = rust_value(&ctx, value);
                let mut state = state.borrow_mut();
                let owner = group.len() == 1;
                if owner {
                    assert_eq!(group, ["Wallbreak"]);
                    assert_eq!(key, "Owner");
                    state.mutations.push(json!(["owner", id, value]));
                } else {
                    assert_eq!(group, ["Wallpaper", IMAGE_PLUGIN, "General"]);
                    assert_eq!(
                        state.desktop(id).plugin,
                        IMAGE_PLUGIN,
                        "image writes require a plugin committed by an earlier invocation"
                    );
                    state.mutations.push(json!(["write", id, key, value]));
                }
                if let Some(fault) = state
                    .fault
                    .filter(|fault| fault.id == id && fault.key == key)
                {
                    return match fault.error {
                        Some(message) => Err(Exception::throw_message(&ctx, message)),
                        None => Ok(()),
                    };
                }
                let desktop = state.desktop_mut(id);
                if owner {
                    desktop.owner = value.as_str().unwrap().into();
                } else {
                    desktop.settings[&key] = value;
                }
                Ok(())
            },
        )
        .unwrap(),
    )
    .unwrap();
    view
}

fn install_api<'js>(ctx: &Ctx<'js>, state: Rc<RefCell<State>>) {
    let globals = ctx.globals();
    let activity_state = state.clone();
    globals
        .set(
            "currentActivity",
            Function::new(ctx.clone(), move || {
                activity_state
                    .borrow_mut()
                    .activities
                    .push("currentActivity".into());
                "current-activity"
            })
            .unwrap(),
        )
        .unwrap();
    let desktops_state = state.clone();
    globals
        .set(
            "desktopsForActivity",
            Function::new(ctx.clone(), move |ctx: Ctx<'js>, activity: String| {
                let mut state = desktops_state.borrow_mut();
                state.activities.push(activity.clone());
                let views: Object = ctx.globals().get("__desktopViews")?;
                let result = Array::new(ctx.clone())?;
                for (index, desktop) in state
                    .desktops
                    .iter()
                    .filter(|desktop| desktop.activity == activity)
                    .enumerate()
                {
                    result.set(index, views.get::<_, JsValue>(desktop.id.to_string())?)?;
                }
                Ok::<_, rquickjs::Error>(result)
            })
            .unwrap(),
        )
        .unwrap();
    let lookup_state = state.clone();
    globals
        .set(
            "desktopById",
            Function::new(ctx.clone(), move |ctx: Ctx<'js>, id: u32| {
                lookup_state.borrow_mut().lookups.push(id);
                ctx.globals()
                    .get::<_, Object>("__desktopViews")?
                    .get::<_, JsValue>(id.to_string())
            })
            .unwrap(),
        )
        .unwrap();
    globals
        .set(
            "print",
            Function::new(ctx.clone(), move |text: String| {
                state.borrow_mut().output.push(text);
            })
            .unwrap(),
        )
        .unwrap();
}

impl Harness {
    fn new(input: Value) -> Self {
        let desktops = input
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| MockDesktop {
                id: entry["id"].as_u64().unwrap().try_into().unwrap(),
                screen: entry["screen"].as_i64().unwrap_or(0),
                activity: entry["activity"]
                    .as_str()
                    .unwrap_or("current-activity")
                    .into(),
                plugin: entry["plugin"].as_str().unwrap_or(IMAGE_PLUGIN).into(),
                owner: entry["owner"].as_str().unwrap_or("").into(),
                settings: entry.get("settings").cloned().unwrap_or_else(|| json!({})),
            })
            .collect();
        Self {
            state: Rc::new(RefCell::new(State {
                desktops,
                ..State::default()
            })),
        }
    }

    fn desktops(&self) -> Vec<MockDesktop> {
        self.state.borrow().desktops.clone()
    }

    fn desktop(&self, id: u32) -> MockDesktop {
        self.state.borrow().desktop(id).clone()
    }

    fn settings(&self, id: u32) -> Value {
        let desktop = self.desktop(id);
        let mut values = defaults();
        for key in KEYS {
            if let Some(value) = desktop.settings.get(key) {
                values[key] = value.clone();
            }
        }
        values
    }

    fn change(&self, id: u32, field: &str, value: Value) {
        let mut state = self.state.borrow_mut();
        if field == "removed" {
            state.desktops.retain(|desktop| desktop.id != id);
            return;
        }
        let desktop = state.desktop_mut(id);
        match field {
            "plugin" => desktop.plugin = value.as_str().unwrap().into(),
            "owner" => desktop.owner = value.as_str().unwrap().into(),
            _ => desktop.settings[field] = value,
        }
    }

    fn snapshot(&self) -> Value {
        self.run(json!({"action": "snapshot"})).unwrap()
    }

    fn run(&self, request: Value) -> std::result::Result<Value, String> {
        self.run_with_commit(request, true)
    }

    fn run_with_commit(
        &self,
        mut request: Value,
        commit_plugins: bool,
    ) -> std::result::Result<Value, String> {
        request
            .as_object_mut()
            .unwrap()
            .entry("owner")
            .or_insert(json!(OWNER));
        request
            .as_object_mut()
            .unwrap()
            .entry("partial")
            .or_insert(json!(false));
        let source = script(&request).unwrap();
        self.state.borrow_mut().output.clear();
        let mut caches = Vec::new();
        let result = {
            let runtime = Runtime::new().unwrap();
            runtime.set_memory_limit(16 * 1024 * 1024);
            runtime.set_max_stack_size(1024 * 1024);
            let deadline = Instant::now() + Duration::from_secs(1);
            runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
            let context = JsContext::full(&runtime).unwrap();
            context.with(|ctx| {
                let globals = ctx.globals();
                let views = Object::new(ctx.clone()).unwrap();
                for desktop in self.desktops() {
                    let cache = Rc::new(RefCell::new(CachedPlugin {
                        plugin: desktop.plugin.clone(),
                        dirty: false,
                    }));
                    views
                        .set(
                            desktop.id.to_string(),
                            wrapper(&ctx, self.state.clone(), &desktop, cache.clone()),
                        )
                        .unwrap();
                    caches.push((desktop.id, cache));
                }
                // Callbacks retain Rust data only. Keeping JS objects in captured
                // Rust closures would hide reference cycles from QuickJS's GC.
                globals.set("__desktopViews", views).unwrap();
                globals.set("__plasmaInjected", false).unwrap();
                install_api(&ctx, self.state.clone());
                let result = ctx
                    .eval::<(), _>(source)
                    .catch(&ctx)
                    .map_err(|error| error.to_string());
                assert!(
                    !globals.get::<_, bool>("__plasmaInjected").unwrap(),
                    "request strings must not execute"
                );
                let prototype: Object = globals
                    .get::<_, Object>("Object")
                    .unwrap()
                    .get("prototype")
                    .unwrap();
                assert!(
                    prototype
                        .get::<_, JsValue>("__plasmaInjected")
                        .unwrap()
                        .is_undefined()
                );
                assert_eq!(
                    rust_value(&ctx, globals.get("request").unwrap()),
                    request,
                    "production JSON.parse injection must round-trip the entire request"
                );
                result
            })
        };
        // Plasma commits the cached setter at invocation teardown, even when
        // evaluation failed. No image writes can rely on this commit beforehand.
        for (id, cache) in caches {
            let cache = cache.borrow();
            if cache.dirty && commit_plugins {
                let mut state = self.state.borrow_mut();
                let old = state.desktop(id).plugin.clone();
                state.commits.push(json!([id, old, cache.plugin]));
                state.desktop_mut(id).plugin = cache.plugin.clone();
            }
        }
        result?;
        let state = self.state.borrow();
        if request["action"] == "snapshot" {
            assert_eq!(
                state.output.len(),
                1,
                "snapshot must print exactly one JSON result"
            );
            Ok(serde_json::from_str(&state.output[0]).unwrap())
        } else {
            assert_eq!(
                state.output,
                ["null"],
                "operations must print one null acknowledgement"
            );
            Ok(Value::Null)
        }
    }

    fn rejects(&self, request: Value, expected: &str) {
        let error = self.run(request).unwrap_err();
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error:?}"
        );
        assert!(
            self.state.borrow().output.is_empty(),
            "failed operations must not acknowledge success"
        );
    }
}

fn changes() -> Vec<(&'static str, Value)> {
    vec![
        ("plugin", json!("org.kde.user-wallpaper")),
        ("Image", json!("file:///user.png")),
        ("FillMode", json!(0)),
        ("Color", json!("#abcdef")),
        ("Blur", json!(true)),
    ]
}

#[test]
fn snapshot_selects_only_connected_desktops_in_current_activity() {
    let definitions = definitions();
    let mut right = definitions[1].clone();
    right["screen"] = json!(3);
    let h = Harness::new(json!([
        definitions[0],
        {"id": 30, "screen": -1, "settings": {"Image": "file:///disconnected.png"}},
        right,
        {"id": 91, "screen": 0, "activity": "other-activity", "settings": {"Image": "file:///other.png"}}
    ]));
    let before = h.desktops();
    assert_eq!(
        h.snapshot(),
        json!([
            {"id": 11, "plugin": "org.kde.slideshow", "settings": {
                "Image": "file:///left.png", "FillMode": 0, "Color": "#112233", "Blur": true
            }},
            {"id": 27, "plugin": IMAGE_PLUGIN, "settings": {
                "Image": "file:///right.png", "FillMode": 1, "Color": "#445566", "Blur": false
            }}
        ])
    );
    let state = h.state.borrow();
    assert_eq!(state.activities, ["currentActivity", "current-activity"]);
    assert!(state.lookups.is_empty());
    assert_eq!(
        state
            .reads
            .iter()
            .map(|read| read["id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [11, 11, 11, 11, 27, 27, 27, 27]
    );
    assert!(state.mutations.is_empty());
    assert_eq!(state.desktops, before);
}

#[test]
fn snapshot_uses_exact_defaults_without_replacing_false_zero_or_empty_values() {
    let explicit = json!({"Image": "", "FillMode": 0, "Color": "", "Blur": false});
    let h = Harness::new(json!([{"id": 1}, {"id": 2, "settings": explicit}]));
    assert_eq!(
        h.snapshot(),
        json!([
            {"id": 1, "plugin": IMAGE_PLUGIN, "settings": defaults()},
            {"id": 2, "plugin": IMAGE_PLUGIN, "settings": explicit}
        ])
    );
    let expected: Vec<_> = [1, 2]
        .into_iter()
        .flat_map(|id| KEYS.map(|key| json!({"id": id, "key": key, "fallback": defaults()[key]})))
        .collect();
    assert_eq!(h.state.borrow().reads, expected);
}

#[test]
fn snapshot_rejects_activity_without_connected_desktops() {
    for input in [
        json!([]),
        json!([{"id": 1, "screen": -1}]),
        json!([{"id": 1, "activity": "other-activity"}]),
    ] {
        let h = Harness::new(input);
        h.rejects(
            json!({"action": "snapshot"}),
            "no active desktop containments",
        );
        assert!(h.state.borrow().mutations.is_empty());
    }
}

#[test]
fn switch_prevalidates_every_desktop_before_mutating_anything() {
    for (field, value) in changes().into_iter().chain([("removed", Value::Null)]) {
        let h = Harness::default();
        let previous = h.snapshot();
        h.change(27, field, value);
        let before = h.desktops();
        let mut request = request("switch", &previous);
        request["partial"] = json!(true);
        h.rejects(request, "wallpaper changed before playback");
        let state = h.state.borrow();
        assert_eq!(state.lookups, [11, 27], "{field}");
        assert!(state.mutations.is_empty(), "{field}");
        assert!(state.commits.is_empty(), "{field}");
        assert_eq!(state.desktops, before, "{field}");
    }
}

#[test]
fn switch_commits_plugins_at_teardown_without_writing_image_settings() {
    let h = Harness::default();
    let previous = h.snapshot();
    let mut before = h.desktops();
    h.run(request("switch", &previous)).unwrap();
    assert_eq!(
        h.state.borrow().mutations,
        vec![
            json!(["owner", 11, OWNER]),
            json!(["plugin", 11, IMAGE_PLUGIN]),
            json!(["owner", 27, OWNER]),
            json!(["plugin", 27, IMAGE_PLUGIN]),
        ]
    );
    assert_eq!(
        h.state.borrow().commits,
        vec![
            json!([11, "org.kde.slideshow", IMAGE_PLUGIN]),
            json!([27, IMAGE_PLUGIN, IMAGE_PLUGIN]),
        ]
    );
    for desktop in &mut before {
        desktop.plugin = IMAGE_PLUGIN.into();
        desktop.owner = OWNER.into();
    }
    assert_eq!(h.desktops(), before);
}

#[test]
fn apply_requires_a_real_plugin_switch_committed_by_a_preceding_invocation() {
    let h = Harness::default();
    let previous = h.snapshot();
    let mut apply = request("apply", &previous);
    apply["partial"] = json!(true);
    h.rejects(apply.clone(), "did not retain");
    assert!(h.state.borrow().mutations.is_empty());
    let mut switch = request("switch", &previous);
    switch["partial"] = json!(true);
    h.run_with_commit(switch, false).unwrap();
    assert_eq!(h.desktop(11).plugin, "org.kde.slideshow");
    assert!(h.state.borrow().commits.is_empty());
    h.state.borrow_mut().mutations.clear();
    h.rejects(apply, "did not retain");
    assert!(h.state.borrow().mutations.is_empty());
    h.run(request("switch", &previous)).unwrap();
    h.state.borrow_mut().mutations.clear();
    h.run(request("apply", &previous)).unwrap();
    let expected: Vec<_> = [11, 27]
        .into_iter()
        .flat_map(|id| KEYS.map(|key| json!(["write", id, key, applied()[key]])))
        .collect();
    assert_eq!(h.state.borrow().mutations, expected);
    for id in [11, 27] {
        assert_eq!(h.settings(id), applied());
    }
}

#[test]
fn apply_prevalidates_all_owners_plugins_and_original_settings_before_first_write() {
    for (field, value) in changes().into_iter().chain([
        ("owner", json!("")),
        ("owner", json!(":1.99")),
        ("removed", Value::Null),
    ]) {
        let h = Harness::default();
        let previous = h.snapshot();
        h.run(request("switch", &previous)).unwrap();
        h.change(27, field, value);
        let before = h.desktops();
        h.state.borrow_mut().mutations.clear();
        let mut request = request("apply", &previous);
        request["partial"] = json!(true);
        h.rejects(request, "did not retain");
        assert!(h.state.borrow().mutations.is_empty(), "{field}");
        assert_eq!(h.desktops(), before, "{field}");
    }
}

#[test]
fn verify_reads_every_applied_setting_without_mutations() {
    let h = Harness::default();
    let previous = h.snapshot();
    h.run(request("switch", &previous)).unwrap();
    h.run(request("apply", &previous)).unwrap();
    let before = h.desktops();
    h.state.borrow_mut().mutations.clear();
    h.state.borrow_mut().reads.clear();
    h.run(request("verify", &previous)).unwrap();
    let expected: Vec<_> = [11, 27]
        .into_iter()
        .flat_map(|id| ["Owner", "Image", "FillMode", "Color", "Blur"].map(|key| json!([id, key])))
        .collect();
    let state = h.state.borrow();
    assert_eq!(
        state
            .reads
            .iter()
            .map(|read| json!([read["id"], read["key"]]))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(state.mutations.is_empty());
    assert_eq!(state.desktops, before);
}

#[test]
fn verify_detects_lost_ownership_changed_plugins_and_strict_value_mismatches() {
    let mut changes = changes();
    changes
        .iter_mut()
        .find(|(key, _)| *key == "FillMode")
        .unwrap()
        .1 = json!("2");
    for (field, value) in changes.into_iter().chain([
        ("owner", json!("")),
        ("owner", json!(":1.99")),
        ("removed", Value::Null),
    ]) {
        let h = Harness::default();
        let previous = h.snapshot();
        h.run(request("switch", &previous)).unwrap();
        h.run(request("apply", &previous)).unwrap();
        h.change(27, field, value);
        let before = h.desktops();
        h.state.borrow_mut().mutations.clear();
        h.rejects(request("verify", &previous), "did not retain");
        assert!(h.state.borrow().mutations.is_empty(), "{field}");
        assert_eq!(h.desktops(), before, "{field}");
    }
}

#[test]
fn apply_rejects_silently_lost_image_or_fill_mode_before_ack_and_allows_partial_rollback() {
    for key in ["Image", "FillMode"] {
        let h = Harness::default();
        let before = h.desktops();
        let previous = h.snapshot();
        let mut switch = request("switch", &previous);
        switch["partial"] = json!(true);
        h.run(switch).unwrap();
        h.state.borrow_mut().fault = Some(WriteFault {
            id: 27,
            key,
            error: None,
        });
        let mut apply = request("apply", &previous);
        apply["partial"] = json!(true);
        h.rejects(
            apply,
            "Plasma did not retain the requested wallpaper settings",
        );
        assert_eq!(h.settings(11), applied());
        let mut expected = applied();
        expected[key] = previous[1]["settings"][key].clone();
        assert_eq!(h.settings(27), expected, "{key}");
        for desktop in h.desktops() {
            assert_eq!(desktop.plugin, IMAGE_PLUGIN);
            assert_eq!(desktop.owner, OWNER);
        }
        h.state.borrow_mut().fault = None;
        let mut restore = request("restore", &previous);
        restore["partial"] = json!(true);
        h.run(restore).unwrap();
        assert_eq!(h.desktops(), before, "{key}");
    }
}

#[test]
fn restore_independently_restores_each_original_plugin_and_image_settings() {
    let h = Harness::default();
    let before = h.desktops();
    let previous = h.snapshot();
    for action in ["switch", "apply", "verify", "restore"] {
        h.run(request(action, &previous)).unwrap();
    }
    assert_eq!(h.desktops(), before);
    for (snapshot, desktop) in previous.as_array().unwrap().iter().zip(&before) {
        assert_eq!(snapshot["plugin"], json!(desktop.plugin));
        assert_eq!(snapshot["settings"], h.settings(desktop.id));
    }
    h.run(request("restore", &previous)).unwrap();
    assert_eq!(
        h.desktops(),
        before,
        "repeated cleanup must not change restored state"
    );
}

#[test]
fn restore_reports_silent_write_failure_without_clearing_owner_or_switching_plugins() {
    let h = Harness::default();
    let before = h.desktops();
    let previous = h.snapshot();
    h.run(request("switch", &previous)).unwrap();
    h.run(request("apply", &previous)).unwrap();
    let untouched = h.desktop(27);
    h.state.borrow_mut().mutations.clear();
    h.state.borrow_mut().commits.clear();
    h.state.borrow_mut().fault = Some(WriteFault {
        id: 11,
        key: "FillMode",
        error: None,
    });
    h.rejects(
        request("restore", &previous),
        "Plasma could not restore the previous wallpaper settings",
    );
    let mut expected = before[0].clone();
    expected.plugin = IMAGE_PLUGIN.into();
    expected.owner = OWNER.into();
    expected.settings["FillMode"] = applied()["FillMode"].clone();
    assert_eq!(h.desktop(11), expected);
    assert_eq!(
        h.state.borrow().mutations,
        KEYS.map(|key| json!(["write", 11, key, previous[0]["settings"][key]]))
    );
    assert!(h.state.borrow().commits.is_empty());
    assert_eq!(h.desktop(27), untouched);
}

#[test]
fn restore_rolls_back_switch_even_when_apply_never_ran() {
    let h = Harness::default();
    let before = h.desktops();
    let previous = h.snapshot();
    for action in ["switch", "restore"] {
        let mut request = request(action, &previous);
        request["partial"] = json!(true);
        h.run(request).unwrap();
    }
    assert_eq!(h.desktops(), before);
}

#[test]
fn restore_preserves_user_changes_and_still_restores_other_desktops() {
    for (field, value) in changes() {
        let h = Harness::default();
        let before = h.desktops();
        let previous = h.snapshot();
        h.run(request("switch", &previous)).unwrap();
        h.run(request("apply", &previous)).unwrap();
        h.change(27, field, value);
        let mut user = h.desktop(27);
        user.owner.clear();
        h.state.borrow_mut().mutations.clear();
        h.run(request("restore", &previous)).unwrap();
        assert_eq!(h.desktop(11), before[0], "{field}");
        assert_eq!(h.desktop(27), user, "{field}");
        assert_eq!(
            h.state
                .borrow()
                .mutations
                .iter()
                .filter(|mutation| mutation[1] == 27)
                .cloned()
                .collect::<Vec<_>>(),
            [json!(["owner", 27, ""])],
            "{field}"
        );
    }
}

#[test]
fn restore_rolls_back_partial_apply_failure_at_every_desktop_and_setting() {
    for id in [11, 27] {
        for key in KEYS {
            let h = Harness::default();
            let before = h.desktops();
            let previous = h.snapshot();
            let mut switch = request("switch", &previous);
            switch["partial"] = json!(true);
            h.run(switch).unwrap();
            h.state.borrow_mut().fault = Some(WriteFault {
                id,
                key,
                error: Some("simulated write failure"),
            });
            let mut apply = request("apply", &previous);
            apply["partial"] = json!(true);
            h.rejects(apply, "simulated write failure");
            if id == 27 {
                assert_eq!(
                    h.settings(11),
                    applied(),
                    "earlier desktop was already written"
                );
            }
            let mut stopped = previous
                .as_array()
                .unwrap()
                .iter()
                .find(|desktop| desktop["id"] == id)
                .unwrap()["settings"]
                .clone();
            for written_key in KEYS {
                if written_key == key {
                    break;
                }
                stopped[written_key] = applied()[written_key].clone();
            }
            assert_eq!(
                h.settings(id),
                stopped,
                "{id} {key}: only preceding writes remain"
            );
            h.state.borrow_mut().fault = None;
            let mut restore = request("restore", &previous);
            restore["partial"] = json!(true);
            h.run(restore).unwrap();
            assert_eq!(h.desktops(), before, "{id} {key}");
        }
    }
}

#[test]
fn restore_skips_removed_desktops_and_cleans_up_survivors() {
    let h = Harness::default();
    let before = h.desktops();
    let previous = h.snapshot();
    h.run(request("switch", &previous)).unwrap();
    h.run(request("apply", &previous)).unwrap();
    h.change(11, "removed", Value::Null);
    h.state.borrow_mut().mutations.clear();
    h.run(request("restore", &previous)).unwrap();
    assert_eq!(h.desktops(), [before[1].clone()]);
    assert!(
        h.state
            .borrow()
            .mutations
            .iter()
            .all(|mutation| mutation[1] == 27)
    );
}

#[test]
fn hostile_image_plugin_and_owner_strings_remain_data_throughout_lifecycle() {
    let hostile = "\"; globalThis.__plasmaInjected = true; print(\"injected\"); //\n\\${notCode}$(touch nope)\u{2028}\u{2029}";
    let image = format!("file:///{hostile}");
    let mut left = defaults();
    left["Image"] = json!(image);
    let mut right = defaults();
    right["Color"] = json!(hostile);
    let h = Harness::new(json!([
        {"id": 1, "plugin": "__proto__", "settings": left},
        {"id": 2, "plugin": hostile, "settings": right}
    ]));
    let before = h.desktops();
    let previous = h.snapshot();
    assert_eq!(previous[0]["plugin"], json!("__proto__"));
    assert_eq!(previous[0]["settings"]["Image"], json!(image));
    assert_eq!(previous[1]["plugin"], json!(hostile));
    let mut requested = applied();
    requested["Image"] = json!(format!("file:///applied-{hostile}"));
    requested["__proto__"] = json!({"__plasmaInjected": true});
    for action in ["switch", "apply", "verify"] {
        h.run(
            json!({"action": action, "previous": previous, "applied": requested, "owner": hostile}),
        )
        .unwrap();
    }
    let mut expected = applied();
    expected["Image"] = requested["Image"].clone();
    for id in [1, 2] {
        assert_eq!(h.settings(id), expected);
        assert_eq!(h.desktop(id).owner, hostile);
    }
    h.run(
        json!({"action": "restore", "previous": previous, "applied": requested, "owner": hostile}),
    )
    .unwrap();
    assert_eq!(h.desktops(), before);
}

#[test]
fn unknown_executable_looking_actions_never_mutate_desktops() {
    let h = Harness::default();
    let previous = h.snapshot();
    let before = h.desktops();
    h.rejects(
        request("restore; globalThis.__plasmaInjected = true;", &previous),
        "Unknown wallbreak Plasma operation",
    );
    assert!(h.state.borrow().mutations.is_empty());
    assert_eq!(h.desktops(), before);
}

#[test]
fn restore_preserves_user_change_back_to_original_setting_after_successful_apply() {
    let h = Harness::default();
    let previous = h.snapshot();
    h.run(request("switch", &previous)).unwrap();
    h.run(request("apply", &previous)).unwrap();
    h.change(27, "FillMode", previous[1]["settings"]["FillMode"].clone());
    let mut user = h.desktop(27);
    user.owner.clear();
    h.state.borrow_mut().mutations.clear();
    h.run(request("restore", &previous)).unwrap();
    assert_eq!(
        h.desktop(27),
        user,
        "old FillMode does not authorize restoring other applied settings"
    );
    assert_eq!(
        h.state
            .borrow()
            .mutations
            .iter()
            .filter(|mutation| mutation[1] == 27)
            .cloned()
            .collect::<Vec<_>>(),
        [json!(["owner", 27, ""])]
    );
}

#[test]
fn rollback_after_rejected_prevalidation_preserves_user_selected_image_plugin() {
    let h = Harness::default();
    let previous = h.snapshot();
    h.change(11, "plugin", json!(IMAGE_PLUGIN));
    let user = h.desktop(11);
    let mut switch = request("switch", &previous);
    switch["partial"] = json!(true);
    h.rejects(switch, "changed before playback");
    assert!(h.state.borrow().mutations.is_empty());
    let mut restore = request("restore", &previous);
    restore["partial"] = json!(true);
    h.run(restore).unwrap();
    assert_eq!(h.desktop(11), user);
    assert!(
        h.state.borrow().mutations.is_empty(),
        "rejected switch claimed no desktops"
    );
}

#[test]
fn different_request_owner_cannot_apply_verify_or_restore_another_clients_desktops() {
    for action in ["apply", "verify", "restore"] {
        let h = Harness::default();
        let previous = h.snapshot();
        h.run(request("switch", &previous)).unwrap();
        if action != "apply" {
            h.run(request("apply", &previous)).unwrap();
        }
        let before = h.desktops();
        h.state.borrow_mut().mutations.clear();
        let mut request = request(action, &previous);
        request["owner"] = json!(":1.99");
        request["partial"] = json!(action == "apply");
        if action == "restore" {
            h.run(request).unwrap();
        } else {
            h.rejects(request, "did not retain");
        }
        assert!(h.state.borrow().mutations.is_empty(), "{action}");
        assert_eq!(
            h.desktops(),
            before,
            "{action}: foreign markers must not be cleared"
        );
    }
}

#[test]
fn restore_skips_absent_or_foreign_ownership_in_both_cleanup_modes() {
    for partial in [false, true] {
        for owner in ["", ":1.99"] {
            let h = Harness::default();
            let original = h.desktops();
            let previous = h.snapshot();
            h.run(request("switch", &previous)).unwrap();
            h.run(request("apply", &previous)).unwrap();
            h.change(27, "owner", json!(owner));
            let unowned = h.desktop(27);
            h.state.borrow_mut().mutations.clear();
            let mut request = request("restore", &previous);
            request["partial"] = json!(partial);
            h.run(request).unwrap();
            assert_eq!(h.desktop(11), original[0], "{partial} {owner}");
            assert_eq!(h.desktop(27), unowned, "{partial} {owner}");
            assert!(
                h.state
                    .borrow()
                    .mutations
                    .iter()
                    .all(|mutation| mutation[1] != 27)
            );
        }
    }
}

#[test]
fn partial_cleanup_restores_only_markers_claimed_before_switch_failure() {
    let h = Harness::default();
    let before = h.desktops();
    let previous = h.snapshot();
    h.state.borrow_mut().fault = Some(WriteFault {
        id: 27,
        key: "Owner",
        error: Some("simulated ownership write failure"),
    });
    let mut switch = request("switch", &previous);
    switch["partial"] = json!(true);
    h.rejects(switch, "simulated ownership write failure");
    assert_eq!(
        h.desktop(11).plugin,
        IMAGE_PLUGIN,
        "failed evaluation still commits cached plugin"
    );
    assert_eq!(h.desktop(11).owner, OWNER);
    assert_eq!(h.desktop(27), before[1]);
    h.state.borrow_mut().fault = None;
    h.state.borrow_mut().mutations.clear();
    let mut restore = request("restore", &previous);
    restore["partial"] = json!(true);
    h.run(restore).unwrap();
    assert_eq!(h.desktops(), before);
    assert!(
        h.state
            .borrow()
            .mutations
            .iter()
            .all(|mutation| mutation[1] == 11)
    );
}
