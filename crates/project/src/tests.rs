use super::*;
use photocraft_cms::Intent;

fn sp(id: &str) -> SpaceId {
    SpaceId::parse(id).unwrap()
}

fn sample() -> Project {
    let mut p = Project::new("Shoot");
    let a = p.add_album("Day 1").unwrap();
    let b = p.add_album("Day 2").unwrap();
    p.add_photos(a, &["C:/pics/a.jpg".into(), "C:/pics/b.jpg".into()], false, Some("2026-10-07T00:00:00Z")).unwrap();
    p.add_photos(b, &["Shoot Media/Day 2/c.jpg".into()], true, None).unwrap();
    p.color.working = Some(sp("acescg"));
    p.album_mut(b).unwrap().color.output = Some(sp("rec709-bt1886"));
    p
}

#[test]
fn ids_are_unique_and_increasing() {
    let p = sample();
    let mut ids: Vec<u64> = p.albums.iter().flat_map(|a| std::iter::once(a.id).chain(a.photos.iter().map(|x| x.id))).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n);
    assert!(p.next_id > *ids.last().unwrap());
}

#[test]
fn albums_add_rename_delete() {
    let mut p = Project::new("  ");
    assert_eq!(p.name, "Untitled");
    let a = p.add_album("Trip").unwrap();
    assert!(p.add_album("trip").is_err(), "names are unique, case-insensitive");
    assert!(p.add_album("   ").is_err());
    assert!(p.add_album(&"x".repeat(300)).is_err());
    let b = p.add_album("Other").unwrap();
    assert!(p.rename_album(b, "TRIP").is_err());
    p.rename_album(a, "Trip").unwrap();
    p.rename_album(a, "Trip 2024").unwrap();
    assert_eq!(p.album(a).unwrap().name, "Trip 2024");
    assert_eq!(p.rename_album(999, "x"), Err(ProjectError::NoAlbum(999)));
    assert_eq!(p.delete_album(a).unwrap().name, "Trip 2024");
    assert!(p.delete_album(a).is_err());
    assert_eq!(p.albums.len(), 1);
}

#[test]
fn photos_dedupe_per_album_remove_and_relink() {
    let mut p = Project::new("P");
    let a = p.add_album("A").unwrap();
    let b = p.add_album("B").unwrap();
    let r = p.add_photos(a, &["C:\\x\\1.jpg".into(), "C:/x/1.jpg".into(), "C:/x/2.jpg".into()], false, None).unwrap();
    let Added::New(first) = r[0] else { panic!("{r:?}") };
    assert_eq!(r[1], Added::Duplicate(first));
    assert!(matches!(r[2], Added::New(_)));
    // Another album may hold the same file.
    assert!(matches!(p.add_photos(b, &["C:/x/1.jpg".into()], false, None).unwrap()[0], Added::New(_)));
    assert_eq!(p.photo_count(), 3);
    assert_eq!(p.find_photo(first).unwrap().0.id, a);
    p.relink(first, "D:/moved/1.jpg", false).unwrap();
    assert_eq!(p.find_photo(first).unwrap().1.path, "D:/moved/1.jpg");
    assert!(p.relink(first, "", false).is_err());
    assert!(p.relink(12345, "D:/x.jpg", false).is_err());
    assert_eq!(p.remove_photo(first).unwrap().id, first);
    assert_eq!(p.remove_photo(first), Err(ProjectError::NoPhoto(first)));
    assert!(p.add_photos(999, &["C:/x.jpg".into()], false, None).is_err());
    // Managed paths stay inside the project folder.
    assert!(p.add_photos(a, &["../escape.jpg".into()], true, None).is_err());
    assert!(p.add_photos(a, &["C:/abs.jpg".into()], true, None).is_err());
    assert!(p.add_photos(a, &["/abs.jpg".into()], true, None).is_err());
}

#[test]
fn colour_resolves_photo_over_album_over_project() {
    let mut p = sample();
    let b = p.albums[1].id;
    let c = p.albums[1].photos[0].id;
    let a_photo = p.albums[0].photos[0].id;
    let base = ColorPipeline::default();
    let r = p.resolve_color(c, &base).unwrap();
    assert_eq!(r.working.as_str(), "acescg");
    assert_eq!(r.output.as_str(), "rec709-bt1886");
    assert_eq!(r.input, InputSpace::Auto);
    let r = p.resolve_color(a_photo, &base).unwrap();
    assert_eq!(r.output.as_str(), "srgb", "album 1 inherits the base output");
    p.color_mut(Level::Photo(c)).unwrap().intent = Some(Intent::Perceptual);
    p.color_mut(Level::Photo(c)).unwrap().output = Some(sp("display-p3"));
    let r = p.resolve_color(c, &base).unwrap();
    assert_eq!(r.output.as_str(), "display-p3");
    assert_eq!(r.intent, Intent::Perceptual);
    assert_eq!(p.resolve_level(Level::Album(b), &base).unwrap().output.as_str(), "rec709-bt1886");
    assert_eq!(p.resolve_level(Level::Project, &base).unwrap().output.as_str(), "srgb");
    assert!(p.resolve_color(999, &base).is_err());
    assert!(p.color_of(Level::Album(999)).is_err());
}

#[test]
fn json_round_trip() {
    let p = sample();
    let text = p.to_json().unwrap();
    assert!(text.contains("\"nextId\""), "{text}");
    assert!(text.contains("\"working\": \"acescg\""), "{text}");
    assert!(text.contains('\n'), "pretty");
    let q = Project::from_json(&text).unwrap();
    assert_eq!(p, q);
}

#[test]
fn from_json_repairs_next_id_and_accepts_minimal_files() {
    let p = Project::from_json(r#"{"version":1,"name":"x","albums":[{"id":7,"name":"A","photos":[{"id":9,"path":"/a.jpg"}]}]}"#).unwrap();
    assert_eq!(p.next_id, 10);
    assert!(!p.albums[0].photos[0].managed);
}

#[test]
fn malformed_project_files_are_errors() {
    let bad = [
        "",
        "null",
        "[]",
        "{",
        r#"{"name":"x"}"#,
        r#"{"version":0,"name":"x"}"#,
        r#"{"version":99,"name":"x"}"#,
        r#"{"version":"1","name":"x"}"#,
        r#"{"version":1,"name":"x","albums":[{"id":1,"name":"A"},{"id":1,"name":"B"}]}"#,
        r#"{"version":1,"name":"x","albums":[{"id":1,"name":"A","photos":[{"id":1,"path":"/a"}]}]}"#,
        r#"{"version":1,"name":"x","albums":[{"id":0,"name":"A"}]}"#,
        r#"{"version":1,"name":"x","albums":[{"id":1,"name":" "}]}"#,
        r#"{"version":1,"name":"x","albums":[{"id":1,"name":"A","photos":[{"id":2,"path":""}]}]}"#,
        r#"{"version":1,"name":"x","albums":[{"id":1,"name":"A","photos":[{"id":2,"path":"../../etc/passwd","managed":true}]}]}"#,
        r#"{"version":1,"name":"x","color":{"working":"no-such-space"}}"#,
        r#"{"version":1,"name":"x","color":{"intent":"sideways"}}"#,
        r#"{"version":1,"name":"x","color":{"bpc":"yes"}}"#,
        r#"{"version":1,"name":"x","albums":[{"id":-1,"name":"A"}]}"#,
    ];
    for t in bad {
        assert!(Project::from_json(t).is_err(), "accepted: {t}");
    }
}

#[test]
fn path_helpers() {
    assert_eq!(sidecar_path("C:/p/IMG_0001.jpg"), "C:/p/IMG_0001.jpg.pcraft");
    assert!(is_sidecar("C:/p/IMG_0001.jpg.pcraft"));
    assert!(!is_sidecar("C:/p/edit.pcraft"));
    assert!(!is_sidecar("C:/p/.x.pcraft"));
    assert_eq!(media_dir("C:/work/MyShoot.pvproj", "Day: 1/2"), "C:/work/MyShoot Media/Day_ 1_2");
    assert_eq!(media_dir("C:\\work\\MyShoot.pvproj", "con"), "C:\\work\\MyShoot Media\\_con");
    assert_eq!(thumb_cache_dir("/w/MyShoot.pvproj"), "/w/MyShoot.pvcache/thumbs");
    assert_eq!(thumb_cache_dir("MyShoot.pvproj"), "MyShoot.pvcache/thumbs");
    assert_eq!(relative_to_project("C:\\work\\MyShoot.pvproj", "C:\\work\\MyShoot Media\\A\\x.jpg").as_deref(), Some("MyShoot Media/A/x.jpg"));
    assert_eq!(relative_to_project("C:/work/MyShoot.pvproj", "C:/elsewhere/x.jpg"), None);
    assert_eq!(relative_to_project("C:/work/MyShoot.pvproj", "C:/work2/x.jpg"), None);
    let ph = Photo { id: 1, path: "MyShoot Media/A/x.jpg".into(), managed: true, color: Default::default(), added: None };
    assert_eq!(photo_file("C:\\work\\MyShoot.pvproj", &ph), "C:\\work\\MyShoot Media\\A\\x.jpg");
    assert_eq!(photo_file("/w/MyShoot.pvproj", &ph), "/w/MyShoot Media/A/x.jpg");
    assert_eq!(unique_name("a.jpg", |n| n == "a.jpg" || n == "a (2).jpg"), "a (3).jpg");
    assert_eq!(unique_name("a", |n| n == "a"), "a (2)");
    assert_eq!(sanitize("..."), "Album");
    assert_eq!(dir_of("/x"), "/");
    assert_eq!(file_stem("C:/a/b.c.pvproj"), "b.c");
}

#[test]
fn timestamps() {
    assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339_utc(1_791_374_096), "2026-10-07T11:54:56Z");
}
