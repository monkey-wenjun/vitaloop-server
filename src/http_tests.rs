use super::*;

#[test]
fn request_body_limit_is_inclusive_and_large_bodies_are_rejected() {
    assert!(!body_too_large(MAX_BODY));
    assert!(body_too_large(MAX_BODY + 1));
}

#[test]
fn tile_paths_are_strict() {
    assert_eq!(
        parse_tile_path("/tiles/14/13660/6745.mvt"),
        Some(Some((14, 13660, 6745)))
    );
    assert_eq!(parse_tile_path("/tiles/14/-1/6745.mvt"), Some(None));
    assert_eq!(parse_tile_path("/tiles/14/13660/6745"), None);
    assert_eq!(parse_tile_path("/not-tiles/14/13660/6745.mvt"), None);
}
