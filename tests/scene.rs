use std::path::PathBuf;

use wallbreak::{
    media::Fit,
    project::{self, MediaKind},
};

/// Compare rendered frames against the shipped preview.
///
/// `crop` selects the compared scene-pixel rectangle (x, y, side) for square
/// previews that are center crops; None compares the whole frame.
fn render_for_similarity(
    root: PathBuf,
    width: u32,
    height: u32,
    crop: Option<(f32, f32, f32)>,
) -> f64 {
    let media = project::resolve(&root, false).unwrap();
    assert_eq!(media.kind, MediaKind::Scene);
    assert!(!media.preview_only);
    let scene = media.scene.clone().unwrap();
    let frame = scene.start_frame(width, height, Fit::Cover).unwrap();
    let preview = image::open(root.join("preview.jpg")).unwrap().into_rgba8();
    let profile_width = media
        .scene_profile
        .as_ref()
        .expect("scene selection must retain its profile")
        .projection[0] as f32;
    let scale = width as f32 / profile_width;
    let (compare_w, compare_h, offset) = match crop {
        Some((cx, cy, side)) => (
            (side * scale) as u32,
            (side * scale) as u32,
            ((cx * scale) as u32, (cy * scale) as u32),
        ),
        None => (width, height, (0, 0)),
    };
    let expected = image::imageops::resize(
        &preview,
        compare_w,
        compare_h,
        image::imageops::FilterType::Triangle,
    );
    let mut best = f64::MAX;
    let mut first = None;
    for time in [0.0, 0.25, 0.5, 0.75, 1.0, 1.5] {
        let rendered = frame.render_frame(time).unwrap();
        assert_eq!(rendered.len(), (width * height * 4) as usize);
        assert!(
            rendered
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255)
        );
        if first.is_none() {
            first = Some(rendered.clone());
        }
        let mut total = 0u64;
        let mut count = 0u64;
        for (y, row) in rendered
            .as_chunks::<4>()
            .0
            .chunks(width as usize)
            .enumerate()
        {
            if y < offset.1 as usize || y >= offset.1 as usize + compare_h as usize {
                continue;
            }
            for (x, pixel) in row.iter().enumerate() {
                if x < offset.0 as usize || x >= offset.0 as usize + compare_w as usize {
                    continue;
                }
                let exp = expected.get_pixel(
                    (x - offset.0 as usize) as u32,
                    (y - offset.1 as usize) as u32,
                );
                // The renderer emits BGRA while image pixels are RGBA.
                total += pixel[0].abs_diff(exp[2]) as u64
                    + pixel[1].abs_diff(exp[1]) as u64
                    + pixel[2].abs_diff(exp[0]) as u64;
                count += 1;
            }
        }
        best = best.min(total as f64 / (count as f64 * 3.0));
    }
    println!("best preview mean abs diff: {best:.3}");
    assert_ne!(first.unwrap(), frame.render_frame(2.0).unwrap());
    best
}

#[test]
#[ignore = "requires WALLPAPER_TEST_SCENE_ROOT pointing to a local Eagle Flag project"]
fn renders_shipped_eagle_flag_with_recognizable_preview_similarity() {
    let Some(root) = std::env::var_os("WALLPAPER_TEST_SCENE_ROOT") else {
        eprintln!("skipping shipped scene check without WALLPAPER_TEST_SCENE_ROOT");
        return;
    };
    let root = PathBuf::from(root);
    let media = project::resolve(&root, false).unwrap();
    let profile = media
        .scene_profile
        .clone()
        .expect("scene selection must retain its profile");
    assert_eq!(profile.projection, [2200, 1080]);
    assert_eq!(profile.layers.len(), 1);
    assert_eq!(profile.layers[0].model, [2048, 1024]);

    let best = render_for_similarity(root, 220, 108, None);
    assert!(
        best < 50.0,
        "rendered Eagle Flag does not resemble its preview: {best:.3}"
    );
}

#[test]
#[ignore = "requires WALLPAPER_TEST_BEACH_ROOT pointing to a local Beach project"]
fn renders_shipped_beach_with_recognizable_preview_similarity() {
    let Some(root) = std::env::var_os("WALLPAPER_TEST_BEACH_ROOT") else {
        eprintln!("skipping shipped scene check without WALLPAPER_TEST_BEACH_ROOT");
        return;
    };
    // Preview is the 1001x1001 center crop of the 1920x1080 scene. The authored
    // preview is brighter than engine-faithful output (see deep_space at 14.1
    // with identical code paths), so this threshold covers composition and
    // animation rather than exact exposure.
    let best = render_for_similarity(PathBuf::from(root), 480, 270, Some((459.5, 39.5, 1001.0)));
    assert!(
        best < 75.0,
        "rendered Beach does not resemble its preview: {best:.3}"
    );
}

#[test]
#[ignore = "requires WALLPAPER_TEST_DEEP_SPACE_ROOT pointing to a local Deep Space project"]
fn renders_shipped_deep_space_with_recognizable_preview_similarity() {
    let Some(root) = std::env::var_os("WALLPAPER_TEST_DEEP_SPACE_ROOT") else {
        eprintln!("skipping shipped scene check without WALLPAPER_TEST_DEEP_SPACE_ROOT");
        return;
    };
    // Preview is the 1079x1079 center crop of the 1920x1080 scene.
    let best = render_for_similarity(PathBuf::from(root), 480, 270, Some((420.5, 0.5, 1079.0)));
    assert!(
        best < 60.0,
        "rendered Deep Space does not resemble its preview: {best:.3}"
    );
}
