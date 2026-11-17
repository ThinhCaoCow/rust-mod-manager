//! The user's settings change how rmm works: files left out of a deploy, copies instead of links,
//! hidden problems, the undo depth and the downloads folder. Its own test binary, since it points
//! rmm's settings at a folder of its own through the environment.
use rmm_core::{Game, Instance, deploy, health, install, ops, settings};
use std::fs;
use std::path::Path;

fn write(path: &Path, bytes: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn settings_change_how_rmm_works() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary, before any thread reads the environment.
    unsafe { std::env::set_var("RMM_CONFIG_DIR", tmp.path().join("config")) };
    let (game, src) = (tmp.path().join("game"), tmp.path().join("src"));
    write(&game.join("SkyrimSE.exe"), "exe");
    for f in ["textures/a.dds", "docs/notes.txt", "meshes/skip/x.nif", "meshes/keep.nif"] {
        write(&src.join("A").join(f), "a");
    }
    let inst = Instance::create(&tmp.path().join("inst"), Game::SkyrimSe, &game, &tmp.path().join("local")).unwrap();
    let a = install::install(&inst, &src.join("A"), None, true, None).unwrap().id;
    let has = |kind: &str| health::check(&inst).unwrap().iter().any(|i| i.kind == kind);

    // Hidden problems.
    assert!(has("not_deployed"));
    settings::set_one("health.hidden", "not_deployed").unwrap();
    assert!(!has("not_deployed"));
    assert!(settings::reset(Some("health.hidden")).unwrap().health.hidden.is_empty());
    assert!(settings::set_one("deploy.method", "symlink").is_err() && settings::set_one("nope.x", "1").is_err());

    // Left out: by name anywhere, or by path inside Data.
    settings::set_one("deploy.exclude", "*.TXT, meshes/skip/*").unwrap();
    deploy::deploy(&inst, false).unwrap();
    let data = game.join("Data");
    assert!(data.join("textures/a.dds").is_file() && data.join("meshes/keep.nif").is_file());
    assert!(!data.join("docs/notes.txt").exists() && !data.join("meshes/skip/x.nif").exists());

    // Copies: links are redone as copies, which then stay put, and the game can't reach the mod.
    settings::set_one("deploy.method", "copy").unwrap();
    assert_eq!(deploy::deploy(&inst, true).unwrap().link.len(), 2);
    deploy::deploy(&inst, false).unwrap();
    assert!(deploy::deploy(&inst, true).unwrap().is_noop());
    fs::write(data.join("textures/a.dds"), "changed by the game").unwrap();
    assert_eq!(fs::read_to_string(inst.mod_dir(&a).join("textures/a.dds")).unwrap(), "a");

    // Undo depth.
    settings::set_one("history.limit", "2").unwrap();
    for on in [false, true, false, true] {
        ops::set_mod_enabled(&inst, &a, on).unwrap();
    }
    assert_eq!(inst.history().unwrap().len(), 2);

    // Downloads folder.
    let mut inst = inst;
    assert!(inst.set_downloads_dir(Some("rel".into())).is_err());
    inst.set_downloads_dir(Some(tmp.path().join("dl"))).unwrap();
    assert_eq!(Instance::open(&inst.root).unwrap().downloads_dir(), tmp.path().join("dl"));
    inst.set_downloads_dir(None).unwrap();
    assert_eq!(Instance::open(&inst.root).unwrap().downloads_dir(), inst.root.join("downloads"));
}
