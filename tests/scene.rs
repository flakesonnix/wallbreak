use std::path::PathBuf;

use wallbreak::{
    media::Fit,
    project::{self, MediaKind},
};

#[test]
#[ignore = "requires WALLPAPER_TEST_SCENE_ROOT pointing to a local Eagle Flag project"]
fn renders_shipped_eagle_flag_with_recognizable_preview_similarity() {
    let Some(root) = std::env::var_os("WALLPAPER_TEST_SCENE_ROOT") else {
        eprintln!("skipping shipped scene check without WALLPAPER_TEST_SCENE_ROOT");
        return;
    };
    let root = PathBuf::from(root);
    let media = project::resolve(&root, false).unwrap();
    assert_eq!(media.kind, MediaKind::Scene);
    assert!(!media.preview_only);
    let profile = media
        .scene_profile
        .clone()
        .expect("scene selection must retain its profile");
    assert_eq!(profile.projection, [2200, 1080]);
    assert_eq!(profile.object.model, [2048, 1024]);
    assert_eq!(profile.textures, ["eagle", "flag_normal", "cloth"]);

    let scene = media.scene.clone().unwrap();
    let frame = scene.start_frame(220, 108, Fit::Cover).unwrap();
    let preview = image::open(root.join("preview.jpg")).unwrap().into_rgba8();
    let expected =
        image::imageops::resize(&preview, 220, 108, image::imageops::FilterType::Triangle);
    let mut best = f64::MAX;
    let mut first = None;
    for time in [0.0, 0.25, 0.5, 0.75, 1.0, 1.5] {
        let rendered = frame.render_frame(time).unwrap();
        assert_eq!(rendered.len(), 220 * 108 * 4);
        assert!(rendered
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 255));
        if first.is_none() {
            first = Some(rendered.clone());
        }
        let mut total = 0u64;
        for (rendered, expected) in rendered.as_chunks::<4>().0.iter().zip(expected.pixels()) {
            // The renderer emits BGRA while image pixels are RGBA.
            total += rendered[0].abs_diff(expected[2]) as u64
                + rendered[1].abs_diff(expected[1]) as u64
                + rendered[2].abs_diff(expected[0]) as u64;
        }
        best = best.min(total as f64 / (220.0 * 108.0 * 3.0));
    }
    println!("best preview mean abs diff: {best:.3}");
    assert_ne!(first.unwrap(), frame.render_frame(2.0).unwrap());
    assert!(
        best < 50.0,
        "rendered Eagle Flag does not resemble its preview: {best:.3}"
    );
}
