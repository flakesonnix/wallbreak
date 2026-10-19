// Executed only through Plasma's evaluateScript API, with JSON-decoded request data.
(function () {
    "use strict";
    const plugin = "org.kde.image";
    const defaults = {Image: "", FillMode: 2, Color: "#ffffff", Blur: false};

    function settings(desktop) {
        desktop.currentConfigGroup = ["Wallpaper", plugin, "General"];
        const result = {};
        for (const key of Object.keys(defaults)) {
            result[key] = desktop.readConfig(key, defaults[key]);
        }
        return result;
    }

    function equal(left, right) {
        return Object.keys(defaults).every(key => left[key] === right[key]);
    }

    function owner(desktop, value) {
        desktop.currentConfigGroup = ["Wallbreak"];
        if (value !== undefined) {
            desktop.writeConfig("Owner", value);
        }
        return desktop.readConfig("Owner", "");
    }

    if (request.action === "snapshot") {
        const targets = desktopsForActivity(currentActivity()).filter(desktop => desktop.screen >= 0);
        if (targets.length === 0) {
            throw new Error("Plasma has no active desktop containments");
        }
        print(JSON.stringify(targets.map(desktop => ({
            id: desktop.id,
            plugin: desktop.wallpaperPlugin,
            settings: settings(desktop)
        }))));
        return;
    }

    const targets = request.previous.map(previous => ({
        previous: previous,
        desktop: desktopById(previous.id)
    }));
    if (request.action === "switch") {
        // Validate every target before changing any of them.
        for (const target of targets) {
            if (!target.desktop || target.desktop.wallpaperPlugin !== target.previous.plugin ||
                    !equal(settings(target.desktop), target.previous.settings)) {
                throw new Error("Plasma wallpaper changed before playback; retry");
            }
        }
        for (const target of targets) {
            owner(target.desktop, request.owner);
            target.desktop.wallpaperPlugin = plugin;
        }
    } else if (request.action === "apply" || request.action === "verify") {
        for (const target of targets) {
            if (!target.desktop || owner(target.desktop) !== request.owner ||
                    target.desktop.wallpaperPlugin !== plugin ||
                    !equal(settings(target.desktop), request.action === "apply"
                        ? target.previous.settings : request.applied)) {
                throw new Error("Plasma did not retain the requested wallpaper settings");
            }
        }
        if (request.action === "apply") {
            for (const target of targets) {
                for (const key of Object.keys(defaults)) {
                    target.desktop.writeConfig(key, request.applied[key]);
                }
            }
            // A successful API call does not prove that every config write stuck.
            // Reject partial writes before the controller commits normal ownership.
            for (const target of targets) {
                if (!equal(settings(target.desktop), request.applied)) {
                    throw new Error("Plasma did not retain the requested wallpaper settings");
                }
            }
        }
    } else if (request.action === "restore") {
        for (const target of targets) {
            const desktop = target.desktop;
            if (!desktop || owner(desktop) !== request.owner) {
                continue;
            }
            const current = settings(desktop);
            // Also admit partial writes after a failed apply. Different values
            // belong to the user; do not replace their plugin or configuration.
            const unchanged = Object.keys(defaults).every(key => current[key] === request.applied[key] ||
                (request.partial && current[key] === target.previous.settings[key]));
            if (desktop.wallpaperPlugin !== plugin || !unchanged) {
                owner(desktop, "");
                continue;
            }
            desktop.currentConfigGroup = ["Wallpaper", plugin, "General"];
            for (const key of Object.keys(defaults)) {
                desktop.writeConfig(key, target.previous.settings[key]);
            }
            if (!equal(settings(desktop), target.previous.settings)) {
                throw new Error("Plasma could not restore the previous wallpaper settings");
            }
            desktop.wallpaperPlugin = target.previous.plugin;
            owner(desktop, "");
        }
    } else {
        throw new Error("Unknown wallbreak Plasma operation");
    }
    print("null");
})();
